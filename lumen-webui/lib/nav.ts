import {
  Activity,
  ArrowUpCircle,
  Boxes,
  Cable,
  Copy,
  Database,
  Gauge,
  HardDrive,
  KeyRound,
  Layers,
  LayoutDashboard,
  Network,
  Route,
  Server,
  ServerCog,
  Settings,
  Share2,
  Shield,
  SlidersHorizontal,
  Spline,
  Waypoints,
  Wrench,
  type LucideIcon,
} from "lucide-react";

/// Where a machine's own page lives. Machines are runtime data rather than
/// nav entries, but the way to address one is still navigation's to define.
export const vmHref = (vmid: number, section = "overview"): string =>
  `/virtual-machines?vm=${vmid}&section=${section}`;

/// A leaf destination inside an expandable section.
export interface NavChild {
  id: string;
  label: string;
  href: string;
  icon: LucideIcon;
}

/// A top-level entry in the left nav. Entries with `children` render as an
/// expandable section whose `href` is the section root (an index route that
/// redirects to the first child); entries without are plain links.
export interface NavItem {
  id: string;
  label: string;
  href: string;
  icon: LucideIcon;
  children?: NavChild[];
}

/// The console's navigation, and the single source of truth for it: the
/// sidebar renders it and the command palette flattens it into "Go to"
/// actions, so a new section is added in exactly one place.
export const NAV: NavItem[] = [
  { id: "dashboard", label: "Dashboard", icon: Gauge, href: "/dashboard" },
  { id: "virtual-machines", label: "Virtual Machines", icon: Boxes, href: "/virtual-machines" },
  {
    id: "networking",
    label: "Networking",
    icon: Network,
    href: "/networking",
    children: [
      { id: "overview",    label: "Overview",    href: "/networking/overview",    icon: LayoutDashboard },
      { id: "interfaces",  label: "Interfaces",  href: "/networking/interfaces",  icon: Cable },
      { id: "networks",    label: "Networks",    href: "/networking/networks",    icon: Waypoints },
      { id: "fabrics",     label: "Fabrics",     href: "/networking/fabrics",     icon: Share2 },
      { id: "routing",     label: "Routing",     href: "/networking/routing",     icon: Route },
      { id: "tunnels",     label: "Tunnels",     href: "/networking/tunnels",     icon: Spline },
      { id: "firewall",    label: "Firewall",    href: "/networking/firewall",    icon: Shield },
      { id: "diagnostics", label: "Diagnostics", href: "/networking/diagnostics", icon: Activity },
    ],
  },
  {
    id: "storage",
    label: "Storage",
    icon: HardDrive,
    href: "/storage",
    children: [
      { id: "pools",   label: "Pools",   href: "/storage/pools",   icon: Database },
      { id: "disks",   label: "Disks",   href: "/storage/disks",   icon: HardDrive },
      { id: "volumes", label: "Volumes", href: "/storage/volumes", icon: Copy },
    ],
  },
  {
    id: "infrastructure",
    label: "Infrastructure",
    icon: Server,
    href: "/infrastructure",
    children: [
      { id: "clusters", label: "Clusters", href: "/infrastructure/clusters", icon: Layers },
      { id: "nodes",    label: "Nodes",    href: "/infrastructure/nodes",    icon: ServerCog },
    ],
  },
  {
    id: "system",
    label: "System",
    icon: Settings,
    href: "/system",
    children: [
      { id: "general", label: "General", href: "/system/general", icon: SlidersHorizontal },
      { id: "authentication", label: "Authentication", href: "/system/authentication", icon: KeyRound },
      { id: "updates", label: "Updates", href: "/system/updates", icon: ArrowUpCircle },
      { id: "maintenance", label: "Maintenance", href: "/system/maintenance", icon: Wrench },
    ],
  },
];

/// Where a section's index route sends the visitor. The static export can't
/// server-redirect, so each section index hops client-side to its first child.
export const firstChildHref = (item: NavItem): string => item.children?.[0]?.href ?? item.href;
