// A message that appears below the visible part of the page (for example the reason a settings
// value was refused, shown under the field that was just used) must not stay hidden behind the
// status bar, which is fixed to the bottom edge on a phone. `scrollIntoView` honours
// `scroll-padding-*` of the page (src/styles/app.css), which keeps the bar's height free.

/** Svelte action: scroll the element into view when it appears, only as far as needed. */
export function reveal(node: HTMLElement) {
  node.scrollIntoView({ block: "nearest", inline: "nearest" });
}
