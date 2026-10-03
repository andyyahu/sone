// Tailwind's @sm, @lg, @3xl, @5xl and @7xl container-query thresholds, in rem.
// Match MediaGrid's ordinary CSS grid so switching to virtual rows is seamless.
const BREAKPOINTS = [24, 32, 48, 64, 80];

export function mediaGridColumns(width: number, rem = 16): number {
  return (
    1 + BREAKPOINTS.filter((breakpoint) => width >= breakpoint * rem).length
  );
}
