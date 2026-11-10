// A message that appears below the visible part of the page (for example the reason a settings
// value was refused, shown under the field that was just used) must not stay hidden behind the
// status bar, which is fixed to the bottom edge on a phone. `scrollIntoView` honours
// `scroll-padding-*` of the page (src/styles/app.css), which keeps the bar's height free.

/**
 * Svelte action: scroll the element into view, only as far as needed. It runs when the element
 * appears and again whenever the parameter changes. A row of the settings page keeps its message
 * element when a second refusal only changes the text, so the page passes the message itself
 * (`use:reveal={problem}`, a new object for every refusal): then a new reason is brought into view
 * too, even when its words are the same as before.
 */
export function reveal(node: HTMLElement, _message?: unknown) {
  const show = () => node.scrollIntoView({ block: "nearest", inline: "nearest" });
  show();
  return { update: show };
}
