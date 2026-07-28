// How a node's name is written in the console.
//
// One place, because every cross-environment table shows the same names and a
// rule about which part of a name is worth screen space is exactly the kind of
// thing that goes wrong when it is written six times.

/// The node's name without its domain.
///
/// `lumen1.ad.quartz.systems` becomes `lumen1`. The domain is identical on
/// every row of every table — a cluster's members share it by construction —
/// so it costs a column's width to say nothing that distinguishes one row from
/// the next. The whole name stays available as a `title`, and is what every
/// request still carries: corosync matches on the full name, so this is a
/// display rule and nothing more.
///
/// An address is left alone. A node named by its IP has no domain to trim, and
/// cutting at the first dot would turn `10.0.0.4` into `10`.
export const shortNodeName = (node: string): string => {
  if (!node) return node;
  if (isAddress(node)) return node;
  const dot = node.indexOf(".");
  return dot > 0 ? node.slice(0, dot) : node;
};

/// Whether the name is an IPv4 address or an IPv6 one — the two shapes that
/// must never be cut at a separator.
const isAddress = (node: string): boolean =>
  node.includes(":") || /^\d{1,3}(\.\d{1,3}){3}$/.test(node);

/// Several nodes, shortened and joined — for the cells that list members.
export const shortNodeNames = (nodes: string[]): string =>
  nodes.map(shortNodeName).join(", ");
