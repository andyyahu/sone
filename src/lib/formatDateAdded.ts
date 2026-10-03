let dateFormatter: Intl.DateTimeFormat | undefined;

/** Share the locale formatter across virtual rows, including remounted rows. */
export function formatDateAdded(dateString?: string): string {
  if (!dateString) return "";
  const date = new Date(dateString);
  const timestamp = date.getTime();
  // Date.toLocaleDateString returned this instead of throwing for bad API data.
  if (Number.isNaN(timestamp)) return "Invalid Date";

  const diffDays = Math.ceil(Math.abs(Date.now() - timestamp) / 86_400_000);
  if (diffDays <= 7) return "This week";
  if (diffDays <= 14) return "Last week";
  if (diffDays <= 30) return "Last month";

  dateFormatter ??= new Intl.DateTimeFormat(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
  return dateFormatter.format(date);
}
