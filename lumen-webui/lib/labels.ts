/// Wire values, spelled the way a table shows them.
///
/// The APIs speak lower case — `bridge`, `yes`, `online` — because that is the
/// value a filter compares and a URL carries. The console shows every one of
/// them with a capital, so the rule lives here rather than being retyped in
/// each column's `render`. Sorting and searching keep using the raw value; only
/// what an operator reads goes through this.
const SPELLINGS: Record<string, string> = {
  vlan: "VLAN",
  dhcp: "DHCP",
  stp: "STP",
  lacp: "LACP",
  unavail: "Unavailable",
};

/// "bridge" → "Bridge", "coming up" → "Coming up", "vlan" → "VLAN".
export function titleCase(value: string): string {
  if (value === "") return value;
  return SPELLINGS[value] ?? value.charAt(0).toUpperCase() + value.slice(1);
}

/// Filter drop-downs show the same words the cells do, while the option value
/// stays the raw one the predicate matches on.
export const titleCaseOptions = (values: string[]): { value: string; label: string }[] =>
  values.map((value) => ({ value, label: titleCase(value) }));
