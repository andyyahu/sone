/** Solid header action shared by playlist, album, artist, mix, and favorites.
 *  A backdrop-filter on these pills is redrawn with the page, including while
 *  a long list scrolls under a still-visible banner. */
export const headerActionClass =
  "flex items-center gap-2 px-6 py-2.5 bg-th-button text-th-text-primary font-bold text-sm rounded-full hover:bg-th-button-hover hover:scale-[1.03] transition-[background-color,scale] duration-150 ease-settle";
