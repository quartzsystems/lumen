//! A shell on the node itself, over a WebSocket — the console every
//! hypervisor appliance has, and the one thing this console could not do.
//!
//! The transport is the machine console's, exactly: session cookie on the
//! upgrade, `Origin` checked by hand, then bytes both ways until an end
//! stops. What differs is the far side. A machine's console is a socket
//! the hypervisor already opened; a node's is a **pseudoterminal this
//! process creates**, with a login session on the other side of it.
//!
//! ## Why the shell directly, and not `login`
//!
//! `login -f` would be the tidier answer: PAM session, utmp entry, the
//! same program the physical console runs. It cannot be used here. From
//! this daemon's SELinux domain (`unconfined_service_t`) executing
//! `/usr/bin/login` fails with EACCES — and so does `su` — while
//! `/bin/bash` execs fine; both are setuid programs the policy does not
//! let a service domain start. Measured on the appliance, not assumed:
//! the console reported "no shell: Permission denied" and the same exec
//! reproduced by hand under the daemon's own context.
//!
//! So the child drops privileges itself and execs the operator's login
//! shell. What that costs is worth stating plainly: **no utmp entry and
//! no PAM session**, so this shell does not appear in `who`, and
//! `loginctl` does not know about it. What it keeps is everything that
//! matters for the shell being *this operator's*: their uid, their
//! groups (so `sudo` and group-gated files behave), their home, and their
//! shell. The audit trail lives where the rest of the appliance's does —
//! the journal records who opened and closed a node shell.
//!
//! No password is asked because the operator already proved who they are:
//! the session cookie on the upgrade is that proof, checked by PAM
//! against this same node moments earlier.
//!
//! ## Who gets one
//!
//! **Administrators only.** Every other route on this appliance is bounded
//! by what its handler does; a shell is bounded by nothing, so it asks the
//! one question the rest of the API can afford not to: is this operator an
//! administrator of this node ([`lumen_sys::ADMIN_GROUP`])? A console user
//! who is not gets the refusal in words rather than a shell.
//!
//! ## One node
//!
//! This serves a shell on **the node the request reached**, and refuses to
//! pretend otherwise. Carrying a pseudoterminal across the peer channel is
//! a second transport with its own lifetime and its own failure modes; a
//! member's own console is one link away and already knows how to do this.
//! The refusal names that address, so the console can offer it.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

use crate::api::console::same_origin;
use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;

/// The shell an account gets when its passwd entry names none.
const FALLBACK_SHELL: &str = "/bin/bash";

/// How much of the shell's output becomes one WebSocket frame.
const CHUNK: usize = 16 * 1024;

/// The size the terminal starts at, before the browser measures itself
/// and says. Not a guess anybody sees: the first thing the viewer sends
/// is a resize.
const START_COLS: u16 = 120;
const START_ROWS: u16 = 32;

/// A resize, as the browser sends it — the one control message in the
/// stream. Everything else is keystrokes, and keystrokes are binary.
#[derive(Debug, serde::Deserialize)]
struct Resize {
    cols: u16,
    rows: u16,
}

/// GET /api/environment/nodes/{node}/shell/ws — a login session on this
/// node.
pub async fn attach(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(node): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    if !state.config.no_tls {
        same_origin(&headers, &uri)?;
    }

    // This node's shell, or none. A member's own console serves its own.
    let here = state.cluster.node();
    if node != here {
        return Err(ApiError::Conflict(format!(
            "A shell runs on the node it belongs to. Open {node}'s own console to reach it — \
             this one can only offer {here}'s."
        )));
    }

    let who = shell_user(&session).await?;

    let upgrade = upgrade.map_err(|rejection| {
        ApiError::BadRequest(format!(
            "A node shell is a WebSocket, and this request could not become one: {rejection}"
        ))
    })?;

    let name = who.name.clone();
    let pty = Pty::open(&who).map_err(|err| {
        tracing::warn!(user = %name, %err, "could not open a node shell");
        ApiError::Conflict(format!("Could not start a shell session: {err}"))
    })?;

    tracing::info!(user = %name, node = %here, "node shell opened");
    Ok(upgrade.on_upgrade(move |socket| async move {
        pump(socket, pty).await;
        tracing::info!(user = %name, "node shell closed");
    }))
}

/// Whose shell it is: the signed-in operator, if this node knows them and
/// they administer it.
///
/// The account has to be local because `login` is: a realm this appliance
/// authenticates against elsewhere is not a user the node can start a
/// session for, and saying so is better than a shell that dies at exec.
async fn shell_user(session: &Session) -> Result<ShellUser, ApiError> {
    let name = session.0.sub.clone();
    let accounts = lumen_sys::state::read(&lumen_sys::AccountFiles::default()).await;
    let account = accounts
        .users
        .iter()
        .find(|user| user.name == name)
        .ok_or_else(|| {
            ApiError::Conflict(format!(
                "\"{name}\" has no account on this node, so there is no session to open. A node \
                 shell runs as the operator who asked for it."
            ))
        })?;
    if !account.administrator && account.uid != 0 {
        return Err(ApiError::Conflict(format!(
            "\"{name}\" is not an administrator of this node. A shell is not bounded by what any \
             one page can do, so it is offered only to accounts in the {} group.",
            lumen_sys::ADMIN_GROUP
        )));
    }
    Ok(ShellUser {
        name,
        uid: account.uid,
        gid: account.gid,
        home: account.home.clone(),
        shell: if account.shell.is_empty() {
            FALLBACK_SHELL.to_string()
        } else {
            account.shell.clone()
        },
    })
}

/// Everything the child needs to become the operator.
#[derive(Debug, Clone)]
struct ShellUser {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
    shell: String,
}

/// The pseudoterminal and the session on the other side of it.
struct Pty {
    master: AsyncFd<OwnedFd>,
    child: libc::pid_t,
}

impl Pty {
    /// Fork a login session onto a fresh pseudoterminal.
    ///
    /// Everything the child needs is built **before** the fork — after it,
    /// only async-signal-safe calls are legal, and this process has other
    /// threads whose locks the child does not hold. So: allocate the
    /// argument vector here, and let the child do nothing but exec it.
    fn open(who: &ShellUser) -> Result<Pty, std::io::Error> {
        let program = std::ffi::CString::new(who.shell.as_str())?;
        // argv[0] with a leading dash is how every shell has been told it
        // is a login shell: it reads the profile files, which is the
        // difference between a usable session and a bare prompt with no
        // PATH worth having.
        let leaf = who.shell.rsplit('/').next().unwrap_or("sh");
        let argv0 = std::ffi::CString::new(format!("-{leaf}"))?;
        let argv: [*const libc::c_char; 2] = [argv0.as_ptr(), std::ptr::null()];

        let home = std::ffi::CString::new(who.home.as_str())?;
        // The environment, built here because the child may not allocate.
        // TERM matters: the browser's terminal is xterm-compatible, and a
        // shell told nothing draws a prompt for a teletype.
        let env: Vec<std::ffi::CString> = vec![
            std::ffi::CString::new("TERM=xterm-256color")?,
            std::ffi::CString::new(format!("HOME={}", who.home))?,
            std::ffi::CString::new(format!("USER={}", who.name))?,
            std::ffi::CString::new(format!("LOGNAME={}", who.name))?,
            std::ffi::CString::new(format!("SHELL={}", who.shell))?,
            std::ffi::CString::new("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin")?,
        ];
        let mut envp: Vec<*const libc::c_char> = env.iter().map(|v| v.as_ptr()).collect();
        envp.push(std::ptr::null());

        // The operator's supplementary groups, resolved before the fork —
        // `getgrouplist` reads /etc/group and allocates, neither of which
        // is safe on the other side of it. Without them a shell loses the
        // group memberships `sudo` and group-gated files are decided by.
        let name = std::ffi::CString::new(who.name.as_str())?;
        let mut groups: Vec<libc::gid_t> = vec![0; 64];
        let mut count = groups.len() as libc::c_int;
        // SAFETY: getgrouplist fills up to `count` entries and rewrites it
        // with the number needed; a too-small buffer returns -1, and the
        // count it leaves is the size to retry with.
        let found =
            unsafe { libc::getgrouplist(name.as_ptr(), who.gid, groups.as_mut_ptr(), &mut count) };
        if found < 0 {
            groups.resize(count.max(1) as usize, 0);
            // SAFETY: as above, now with the buffer it asked for.
            unsafe { libc::getgrouplist(name.as_ptr(), who.gid, groups.as_mut_ptr(), &mut count) };
        }
        groups.truncate(count.max(0) as usize);
        // A terminal the browser has not measured yet — the viewer's first
        // message replaces it.
        let size = libc::winsize {
            ws_row: START_ROWS,
            ws_col: START_COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // Copied out before the fork: the child may not touch `who`.
        let (who_uid, who_gid) = (who.uid, who.gid);
        let mut master: RawFd = -1;
        // SAFETY: forkpty writes the master fd through the pointer and
        // returns a pid; both outcomes are checked below.
        let pid = unsafe {
            libc::forkpty(
                &mut master,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size as *const libc::winsize as *mut libc::winsize,
            )
        };
        if pid < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid == 0 {
            // The child. forkpty has already made this a session leader
            // with the slave as its controlling terminal and as all three
            // standard descriptors, so what is left is becoming the
            // operator and starting their shell — syscalls only, in the
            // one order that cannot go wrong: groups and gid before uid,
            // because dropping uid first would forfeit the privilege the
            // other two need.
            //
            // SAFETY: every call below is a syscall on values built before
            // the fork; execve replaces the image, and a failure exits
            // rather than returning into a runtime this process no longer
            // shares with anyone.
            unsafe {
                libc::setgroups(groups.len(), groups.as_ptr());
                libc::setgid(who_gid);
                libc::setuid(who_uid);
                // A shell that starts somewhere the operator cannot read
                // is a confusing shell; / is the honest fallback.
                if libc::chdir(home.as_ptr()) != 0 {
                    libc::chdir(c"/".as_ptr());
                }
                libc::execve(program.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }

        // SAFETY: forkpty returned a fresh master descriptor we now own.
        let owned = unsafe { OwnedFd::from_raw_fd(master) };
        set_nonblocking(owned.as_raw_fd())?;
        Ok(Pty {
            master: AsyncFd::new(owned)?,
            child: pid,
        })
    }

    /// Tell the session how big its terminal is. A shell that never hears
    /// this draws its prompt for somebody else's window.
    fn resize(&self, cols: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: an ioctl on our own master fd with a struct we own; a
        // failure here costs a badly drawn prompt, not correctness.
        unsafe {
            libc::ioctl(self.master.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &size);
        }
    }
}

impl Drop for Pty {
    /// The session goes with the connection. A hangup is what a shell
    /// expects when its terminal disappears, and reaping keeps the node
    /// from collecting a zombie per closed browser tab.
    fn drop(&mut self) {
        // SAFETY: signalling and reaping a child this struct owns.
        unsafe {
            libc::kill(self.child, libc::SIGHUP);
            let mut status = 0;
            libc::waitpid(self.child, &mut status, 0);
        }
    }
}

fn set_nonblocking(fd: RawFd) -> Result<(), std::io::Error> {
    // SAFETY: reading and setting flags on a descriptor we own.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Copy bytes both ways until either end stops — the machine console's
/// pump, with one addition: a text frame is a control message rather than
/// more keystrokes, because a terminal has a size and a browser is the
/// only one who knows it.
async fn pump(socket: WebSocket, pty: Pty) {
    let (mut to_browser, mut from_browser) = socket.split();
    let pty = Arc::new(pty);

    let browser_to_shell = {
        let pty = Arc::clone(&pty);
        async move {
            while let Some(Ok(message)) = from_browser.next().await {
                let bytes = match message {
                    Message::Binary(bytes) => bytes,
                    // The one structured message in the stream. Anything
                    // else that arrives as text is typed input.
                    Message::Text(text) => match serde_json::from_str::<Resize>(&text) {
                        Ok(resize) => {
                            pty.resize(resize.cols, resize.rows);
                            continue;
                        }
                        Err(_) => text.as_bytes().to_vec().into(),
                    },
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => continue,
                };
                let mut written = 0;
                while written < bytes.len() {
                    let Ok(mut guard) = pty.master.writable().await else {
                        return;
                    };
                    match guard.try_io(|fd| {
                        // SAFETY: writing the caller's bytes to our own fd.
                        let count = unsafe {
                            libc::write(
                                fd.get_ref().as_raw_fd(),
                                bytes[written..].as_ptr().cast(),
                                bytes.len() - written,
                            )
                        };
                        if count < 0 {
                            Err(std::io::Error::last_os_error())
                        } else {
                            Ok(count as usize)
                        }
                    }) {
                        Ok(Ok(count)) => written += count,
                        Ok(Err(_)) => return,
                        // Not actually writable; the loop waits again.
                        Err(_would_block) => continue,
                    }
                }
            }
        }
    };

    let shell_to_browser = {
        let pty = Arc::clone(&pty);
        async move {
            let mut buffer = vec![0u8; CHUNK];
            loop {
                let Ok(mut guard) = pty.master.ready(Interest::READABLE).await else {
                    break;
                };
                let read = match guard.try_io(|fd| {
                    // SAFETY: reading into a buffer we own from our own fd.
                    let count = unsafe {
                        libc::read(
                            fd.get_ref().as_raw_fd(),
                            buffer.as_mut_ptr().cast(),
                            buffer.len(),
                        )
                    };
                    if count < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(count as usize)
                    }
                }) {
                    // The session ended: the shell exited and the slave
                    // side closed, which reads as EOF (or EIO on Linux).
                    Ok(Ok(0)) | Ok(Err(_)) => break,
                    Ok(Ok(count)) => count,
                    Err(_would_block) => continue,
                };
                let frame = Message::Binary(buffer[..read].to_vec().into());
                if to_browser.send(frame).await.is_err() {
                    break;
                }
            }
            let _ = to_browser.send(Message::Close(None)).await;
        }
    };

    tokio::select! {
        _ = browser_to_shell => {}
        _ = shell_to_browser => {}
    }
}
