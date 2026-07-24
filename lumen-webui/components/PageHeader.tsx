import type { ReactNode } from "react";

/// Console page chrome, in the shape QuartzFire's console uses: a title block
/// pinned to the top of the pane and a body that scrolls under it, so the
/// heading and its controls stay put however long the table below gets.
///
///     <Page>
///       <PageHeader title="Interfaces" description="…" actions={…} />
///       <PageBody>…</PageBody>
///     </Page>
export function Page({ children }: { children: ReactNode }) {
  return <div className="flex flex-col h-full">{children}</div>;
}

/// Standard page title block. `actions` renders flush right (buttons, badges).
export function PageHeader({
  title,
  description,
  actions,
}: {
  title: string;
  description?: string;
  actions?: ReactNode;
}) {
  return (
    <div className="px-[36px] pt-[28px] pb-5 flex-shrink-0 flex items-start justify-between gap-4">
      <div className="min-w-0">
        <h1
          className="text-[28px] font-bold text-[var(--qz-fg-1)] m-0"
          style={{ letterSpacing: "-0.015em" }}
        >
          {title}
        </h1>
        {description && <p className="text-[13px] text-[var(--qz-fg-4)] mt-1 mb-0">{description}</p>}
      </div>
      {actions && <div className="flex items-center gap-2 flex-shrink-0">{actions}</div>}
    </div>
  );
}

/// Everything below the header. Scrolls on its own.
export function PageBody({ children }: { children: ReactNode }) {
  return <div className="flex-1 overflow-auto px-[36px] pb-[28px]">{children}</div>;
}
