import type { Metadata } from "next";
import type { ReactNode } from "react";
import localFont from "next/font/local";
import "./globals.css";

// Manrope ships in-repo (branding/fonts, copied here) so appliance builds
// never fetch fonts from the network. JetBrains Mono isn't vendored yet;
// --qz-font-mono falls through to the system mono stack until a console
// view needs it.
const manrope = localFont({
  src: "../fonts/Manrope-Variable.ttf",
  variable: "--font-manrope",
  weight: "200 800",
  display: "swap",
});

export const metadata: Metadata = {
  title: "Lumen",
  description: "Lumen hypervisor console",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html
      lang="en"
      className={`${manrope.variable} h-full`}
      style={
        {
          "--qz-font-sans": "var(--font-manrope), ui-sans-serif, system-ui, sans-serif",
        } as React.CSSProperties
      }
    >
      <body className="h-full antialiased">{children}</body>
    </html>
  );
}
