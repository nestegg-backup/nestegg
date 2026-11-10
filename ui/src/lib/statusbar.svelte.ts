// Short messages in the status bar ("Gespeichert um 08:14: Rhythmus: Täglich").
// The status bar announces them through an aria-live region.

export type BarTone = "ok" | "bad";

class StatusBarState {
  message = $state<{ text: string; tone: BarTone } | null>(null);
  #timer: ReturnType<typeof setTimeout> | null = null;

  show(text: string, tone: BarTone = "ok", ms = 10_000) {
    this.message = { text, tone };
    if (this.#timer) clearTimeout(this.#timer);
    this.#timer = setTimeout(() => (this.message = null), ms);
  }
}

export const statusBar = new StatusBarState();
