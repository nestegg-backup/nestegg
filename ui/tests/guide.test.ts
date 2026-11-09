// The setup and key guide: which step is current for which /status, and the commands it shows.
import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import {
  ENABLE_TIMERS_COMMAND,
  PAPER_KEY_COMMAND,
  hasPlaceholder,
  initCommand,
  keyConfirmCodeCommand,
  keyConfirmFileCommand,
  keyExportCommand,
  keyMissing,
  normalizeCheckCode,
  setupGuide,
  shellQuote,
  splitServer,
  stickFile,
  type Guide,
} from "../src/lib/derive";
import { createFormatter } from "../src/lib/format";
import { selfRowText, selfSummary } from "../src/lib/text";
import type { Run, Status } from "../src/lib/types";

const f = createFormatter("de", "de-DE", "Europe/Berlin");
const now = new Date("2026-11-08T18:00:00Z");
const ok: Run = { id: "a", trigger: "timer", state: "succeeded", started_at: "2026-11-08T00:42:00Z" };

type Enc = Status["encryption"];

/** A fully set up PC; each test changes the part it is about. */
function status(over: Partial<Status> = {}, enc: Partial<Enc> = {}, pbs: Partial<Status["pbs"]> = {}, schedule: Partial<Status["schedule"]> = {}): Status {
  return {
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups", credentials_stored: true, ...pbs },
    schedule: { enabled: true, ...schedule },
    encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: true, key_present: true, ...enc },
    health: "ok",
    ...over,
  };
}

const current = (s: Status) => setupGuide(s)?.current ?? null;

describe("setupGuide: one step after the other", () => {
  it("1 connect: nestegg init has not run", () => {
    const g = setupGuide(status({ health: "unconfigured" }, { key_fingerprint: null, key_exported: false, key_present: false }, { credentials_stored: false }, { enabled: false }));
    expect(g).toEqual({ steps: ["connect", "credentials", "key_create", "key_store", "automatic", "first_backup"], current: "connect" });
  });

  it("2 credentials: configured, but the token secret is not stored; also when everything else is missing", () => {
    expect(current(status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: false }, { credentials_stored: false }))).toBe("credentials");
    expect(current(status({ health: "never" }, {}, { credentials_stored: false }))).toBe("credentials");
    // The guide stays after init: this is the case that used to say "everything is ready".
    expect(current(status({ health: "failing" }, {}, { credentials_stored: false }))).toBe("credentials");
  });

  it("3 key_create: encryption on and no key (key_present false)", () => {
    const s = status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: false });
    expect(current(s)).toBe("key_create");
    expect(keyMissing(s)).toBe(true);
  });

  it("3 key_create: without key_present the old rule applies (no fingerprint, copy not confirmed)", () => {
    expect(current(status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: undefined }))).toBe("key_create");
    expect(current(status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: null }))).toBe("key_create");
  });

  it("a key that exists but has no fingerprint never leads to 'create the key' (key create would refuse, --force destroys)", () => {
    const s = status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: true });
    expect(keyMissing(s)).toBe(false);
    expect(current(s)).toBe("key_store");
    // Same when the key is there and its copy is confirmed: nothing to create.
    expect(current(status({ health: "ok", last_success: ok }, { key_fingerprint: null, key_exported: true, key_present: true }))).toBeNull();
  });

  it("4 key_store: the key exists, the copy is not confirmed", () => {
    const g = setupGuide(status({ health: "never" }, { key_exported: false }));
    expect(g?.current).toBe("key_store");
    expect(g?.steps).toEqual(["connect", "credentials", "key_create", "key_store", "automatic", "first_backup"]);
  });

  it("5 automatic: the timer is off while the setup is not finished", () => {
    expect(current(status({ health: "never" }, {}, {}, { enabled: false }))).toBe("automatic");
    // Earlier steps come first, the step stays in the list.
    const s = status({ health: "never" }, { key_exported: false }, {}, { enabled: false });
    expect(setupGuide(s)).toMatchObject({ current: "key_store" });
    expect(setupGuide(s)?.steps).toContain("automatic");
  });

  it("5 automatic: a PC with a backup that has the timer off on purpose is not nagged", () => {
    expect(current(status({ health: "ok", last_success: ok }, {}, {}, { enabled: false }))).toBeNull();
    // ... but while another step is open the list shows the timer as well.
    const s = status({ health: "failing", last_success: ok }, { key_exported: false }, {}, { enabled: false });
    expect(setupGuide(s)).toEqual({ steps: ["connect", "credentials", "key_create", "key_store", "automatic"], current: "key_store" });
  });

  it("6 first_backup: everything is in place", () => {
    expect(current(status({ health: "never" }))).toBe("first_backup");
  });

  it("moves on by itself: every /status gives the next step", () => {
    const order = [
      status({ health: "unconfigured" }, { key_fingerprint: null, key_exported: false, key_present: false }, { credentials_stored: false }, { enabled: false }),
      status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: false }, { credentials_stored: false }, { enabled: false }),
      status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: false }, {}, { enabled: false }),
      status({ health: "never" }, { key_exported: false }, {}, { enabled: false }),
      status({ health: "never" }, {}, {}, { enabled: false }),
      status({ health: "never" }),
      status({ health: "ok", last_success: ok }),
    ].map(current);
    expect(order).toEqual(["connect", "credentials", "key_create", "key_store", "automatic", "first_backup", null]);
  });
});

describe("setupGuide: encryption off and unknown values", () => {
  it("without encryption the key steps are left out and never current", () => {
    const g = setupGuide(status({ health: "never" }, { enabled: false, key_fingerprint: null, key_exported: false, key_present: false }));
    expect(g).toEqual({ steps: ["connect", "credentials", "automatic", "first_backup"], current: "first_backup" });
    expect(current(status({ health: "never" }, { enabled: false, key_fingerprint: null, key_exported: false, key_present: false }, { credentials_stored: false }))).toBe("credentials");
    expect(current(status({ health: "never" }, { enabled: false, key_present: null, key_exported: undefined }, {}, { enabled: false }))).toBe("automatic");
  });

  it("unknown values (null or absent) never count as missing", () => {
    const unknown = status({ health: "never" }, { key_present: null }, { credentials_stored: null });
    expect(current(unknown)).toBe("first_backup");
    const absent = status({ health: "never" }, { key_present: undefined }, { credentials_stored: undefined });
    expect(current(absent)).toBe("first_backup");
    // key_exported absent (older daemon): not "unconfirmed".
    expect(current(status({ health: "never" }, { key_exported: undefined, key_present: undefined }))).toBe("first_backup");
    expect(current(status({ health: "ok", last_success: ok }, { key_present: null }, { credentials_stored: null }))).toBeNull();
  });

  it("a PC with backups but an unconfirmed key copy gets the key step without 'first backup'", () => {
    const g = setupGuide(status({ health: "failing", last_success: ok }, { key_exported: false }));
    expect(g).toEqual({ steps: ["connect", "credentials", "key_create", "key_store", "automatic"], current: "key_store" });
  });

  it("the stored-secret step also shows for a PC that already has backups (secret removed later)", () => {
    expect(current(status({ health: "failing", last_success: ok }, {}, { credentials_stored: false }))).toBe("credentials");
  });

  it("is not shown while a backup runs, and not for a healthy PC", () => {
    expect(setupGuide(status({ health: "running" }))).toBeNull();
    expect(setupGuide(status({ health: "never", current_run: { ...ok, state: "uploading" } }, {}, { credentials_stored: false }))).toBeNull();
    expect(setupGuide(status())).toBeNull();
  });

  it("every combination gives a current step that is in the list of steps", () => {
    const bools = [true, false, null, undefined];
    let n = 0;
    for (const health of ["unconfigured", "never", "ok", "failing"] as const) {
      for (const enabled of [true, false]) {
        for (const cred of bools) {
          for (const present of bools) {
            for (const exported of [true, false, undefined]) {
              for (const fp of ["<FINGERPRINT>", null]) {
                for (const sched of [true, false]) {
                  const s = status(
                    { health, ...(health === "ok" || health === "failing" ? { last_success: ok } : {}) },
                    { enabled, key_present: present, key_exported: exported, key_fingerprint: fp },
                    { credentials_stored: cred },
                    { enabled: sched },
                  );
                  const g: Guide | null = setupGuide(s);
                  n++;
                  if (g) expect(g.steps).toContain(g.current);
                  // Key steps only with encryption (or before setup, when it is not known yet).
                  if (g && !enabled && health !== "unconfigured") expect(g.steps).not.toContain("key_create");
                  // "Create the key" is never the step for a key that exists.
                  if (g?.current === "key_create") expect(present).not.toBe(true);
                }
              }
            }
          }
        }
      }
    }
    expect(n).toBeGreaterThan(500);
  });
});

describe("the page says which step it is", () => {
  it("no key at all differs from an unconfirmed copy", () => {
    const noKey = status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: false });
    expect(selfSummary(noKey, de, f, now).lead).toBe("Es gibt noch keinen Schlüssel für die Sicherungen.");
    expect(selfRowText(noKey, de, f, now)).toBe("Schlüssel fehlt noch");
    const unconfirmed = status({ health: "never" }, { key_exported: false });
    expect(selfSummary(unconfirmed, de, f, now).lead).toBe("Sicherungen warten auf die Bestätigung der Schlüsselkopie.");
    expect(selfRowText(unconfirmed, en, f, now)).toBe("Key copy not confirmed");
  });

  it("without the token secret the lead and the row say so, not 'key copy' or 'start the first backup'", () => {
    for (const s of [
      status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: false }, { credentials_stored: false }),
      status({ health: "never" }, {}, { credentials_stored: false }),
    ]) {
      expect(selfSummary(s, en, f, now).lead).toBe(en.self.credentialsLead);
      expect(selfSummary(s, de, f, now).lead).toBe(de.self.credentialsLead);
      expect(selfRowText(s, en, f, now)).toBe("Token secret missing");
      expect(selfSummary(s, en, f, now).text).not.toMatch(/start the first backup/);
    }
  });

  it("an existing key without fingerprint is 'copy not confirmed', not 'key missing'", () => {
    const s = status({ health: "never" }, { key_fingerprint: null, key_exported: false, key_present: true });
    expect(selfSummary(s, en, f, now).lead).toBe("Backups are waiting for the key copy to be confirmed.");
  });
});

describe("shellQuote and splitServer", () => {
  it("quotes what the shell would misread: the token ID contains !", () => {
    expect(shellQuote("backups")).toBe("backups");
    expect(shellQuote("pbs.example.com")).toBe("pbs.example.com");
    expect(shellQuote("backup@pbs!desktop")).toBe("'backup@pbs!desktop'");
    expect(shellQuote("it's here")).toBe("'it'\\''s here'");
    expect(shellQuote("a b")).toBe("'a b'");
    expect(shellQuote("$(rm -rf x)")).toBe("'$(rm -rf x)'");
    expect(shellQuote("[2001:db8::1]")).toBe("'[2001:db8::1]'");
  });

  it("splits a port off only from host:port and [addr]:port, and brackets a bare IPv6 address", () => {
    expect(splitServer("pbs.example.com")).toEqual({ server: "pbs.example.com", port: null });
    expect(splitServer(" pbs.example.com:8007 ")).toEqual({ server: "pbs.example.com", port: "8007" });
    expect(splitServer("192.0.2.10:8007")).toEqual({ server: "192.0.2.10", port: "8007" });
    expect(splitServer("[2001:db8::1]:8007")).toEqual({ server: "[2001:db8::1]", port: "8007" });
    expect(splitServer("[2001:db8::1]")).toEqual({ server: "[2001:db8::1]", port: null });
    expect(splitServer("2001:db8::1")).toEqual({ server: "[2001:db8::1]", port: null });
    expect(splitServer("2001:db8:0:0:0:0:0:1")).toEqual({ server: "[2001:db8:0:0:0:0:0:1]", port: null });
    // An impossible port is not cut off; the program will say what is wrong.
    expect(splitServer("pbs.example.com:70000")).toEqual({ server: "pbs.example.com:70000", port: null });
    expect(splitServer("")).toEqual({ server: "", port: null });
  });
});

describe("init command", () => {
  it("composes the documented command, every value in --option=value form", () => {
    expect(initCommand({ server: "pbs.example.com", datastore: "backups", namespace: "desktops", authId: "backup@pbs!desktop" })).toBe(
      "sudo nestegg init \\\n  --server=pbs.example.com \\\n  --datastore=backups \\\n  --auth-id='backup@pbs!desktop' \\\n  --namespace=desktops",
    );
  });

  it("leaves out an empty namespace and splits host:port", () => {
    const cmd = initCommand({ server: " pbs.example.com:8007 ", datastore: "backups", namespace: " ", authId: "backup@pbs!desktop" });
    expect(cmd).toContain("--server=pbs.example.com \\\n  --port=8007 \\\n");
    expect(cmd).not.toContain("--namespace");
  });

  it("keeps IPv6 in square brackets, as Config::validate demands", () => {
    const base = { datastore: "b", namespace: "", authId: "a@b!c" };
    expect(initCommand({ ...base, server: "[2001:db8::1]:8007" })).toContain("--server='[2001:db8::1]' \\\n  --port=8007");
    expect(initCommand({ ...base, server: "[2001:db8::1]" })).toContain("--server='[2001:db8::1]' \\\n  --datastore");
    // A bare address gets its brackets and is not mistaken for host:port.
    expect(initCommand({ ...base, server: "2001:db8::1" })).toContain("--server='[2001:db8::1]' \\\n  --datastore");
    expect(initCommand({ ...base, server: "2001:db8::1" })).not.toContain("--port");
  });

  it("a value that starts with - can never be read as an option", () => {
    const cmd = initCommand({ server: "pbs.example.com", datastore: "-backups", namespace: "-x", authId: "-a@b!c" });
    expect(cmd).toContain("--datastore=-backups");
    expect(cmd).toContain("--namespace=-x");
    expect(cmd).toContain("--auth-id='-a@b!c'");
    // No option is ever followed by its value as a separate word.
    expect(cmd).not.toMatch(/--(server|port|datastore|auth-id|namespace) /);
  });

  it("shows placeholders for what is still missing, and never a secret option", () => {
    const cmd = initCommand({ server: "", datastore: "", namespace: "", authId: "" });
    expect(cmd).toBe("sudo nestegg init \\\n  --server=<SERVER> \\\n  --datastore=<DATASTORE> \\\n  --auth-id=<TOKEN-ID>");
    expect(cmd).not.toMatch(/password|secret/i);
  });
});

describe("placeholders can never run", () => {
  it("recognises <NAME> and nothing else", () => {
    expect(hasPlaceholder("sudo nestegg key export <STICK-PATH>")).toBe(true);
    expect(hasPlaceholder("sudo nestegg init --server=<SERVER>")).toBe(true);
    expect(hasPlaceholder("sudo nestegg key confirm-exported --paper-code=<CODE>")).toBe(true);
    expect(hasPlaceholder("sudo nestegg check")).toBe(false);
    expect(hasPlaceholder(PAPER_KEY_COMMAND)).toBe(false);
    expect(hasPlaceholder(ENABLE_TIMERS_COMMAND)).toBe(false);
  });

  it("an empty field leaves a placeholder in the command, a filled one does not", () => {
    const full = { server: "pbs.example.com", datastore: "backups", namespace: "", authId: "backup@pbs!desktop" };
    expect(hasPlaceholder(initCommand(full))).toBe(false);
    for (const key of ["server", "datastore", "authId"] as const) {
      expect(hasPlaceholder(initCommand({ ...full, [key]: " " }))).toBe(true);
    }
    // A token ID is no longer a quoted, valid-looking '<USER@REALM!TOKEN>'.
    expect(initCommand({ ...full, authId: "" })).not.toContain("'<");
  });

  it("the placeholder is not quoted, so a shell stops at it instead of using it as a value", () => {
    for (const cmd of [initCommand({ server: "", datastore: "", namespace: "", authId: "" }), keyExportCommand(""), keyConfirmFileCommand(""), keyConfirmCodeCommand("")]) {
      for (const m of cmd.matchAll(/<[A-Z][A-Z0-9-]*>/g)) {
        const before = cmd[m.index! - 1];
        expect(before === "'" || before === '"').toBe(false);
      }
    }
  });

  it("stick path: only an absolute path completes the commands", () => {
    expect(stickFile("/media/alice/STICK")).toBe("/media/alice/STICK/nestegg-key.json");
    expect(stickFile(" /media/alice/STICK/ ")).toBe("/media/alice/STICK/nestegg-key.json");
    expect(stickFile("STICK")).toBeNull();
    expect(stickFile("")).toBeNull();
    expect(keyExportCommand("/media/alice/STICK")).toBe("sudo nestegg key export /media/alice/STICK/nestegg-key.json");
    expect(keyConfirmFileCommand("/media/alice/STICK")).toBe("sudo nestegg key confirm-exported \\\n  --file=/media/alice/STICK/nestegg-key.json");
    // A USB stick copy needs no --accept-local-copy.
    expect(keyConfirmFileCommand("/media/alice/STICK")).not.toContain("accept-local-copy");
    expect(keyConfirmFileCommand("/media/alice/My Stick")).toBe("sudo nestegg key confirm-exported \\\n  --file='/media/alice/My Stick/nestegg-key.json'");
    expect(hasPlaceholder(keyExportCommand("/media/alice/STICK"))).toBe(false);
    expect(hasPlaceholder(keyExportCommand("STICK"))).toBe(true);
    expect(hasPlaceholder(keyConfirmFileCommand(""))).toBe(true);
  });

  it("check code: eight hex digits in any case, with or without hyphen, otherwise a placeholder", () => {
    expect(normalizeCheckCode("abcd-0123")).toBe("ABCD-0123");
    expect(normalizeCheckCode(" ABCD 0123 ")).toBe("ABCD-0123");
    expect(normalizeCheckCode("abcd0123")).toBe("ABCD-0123");
    expect(normalizeCheckCode("ABCD-012")).toBeNull();
    expect(normalizeCheckCode("ABCD-012G")).toBeNull();
    expect(normalizeCheckCode("")).toBeNull();
    expect(keyConfirmCodeCommand("abcd0123")).toBe("sudo nestegg key confirm-exported \\\n  --paper-code=ABCD-0123");
    expect(hasPlaceholder(keyConfirmCodeCommand("abcd0123"))).toBe(false);
    expect(hasPlaceholder(keyConfirmCodeCommand("nope"))).toBe(true);
  });

  it("the paper key command writes html, as the program needs --format html for that", () => {
    expect(PAPER_KEY_COMMAND).toBe('sudo nestegg key paperkey \\\n  --format=html \\\n  --output="$HOME/nestegg-paper-key.html"');
  });

  it("the timers command enables both timers", () => {
    expect(ENABLE_TIMERS_COMMAND).toBe("sudo systemctl enable --now nestegg-backup.timer nestegg-notify.timer");
  });
});

describe("guide wording", () => {
  it("never asks for a secret on the page and says that a copy on this computer does not count", () => {
    expect(de.guide.setupIntro).toContain("nie hier auf der Seite");
    expect(en.guide.setupIntro).toContain("never type the secret");
    expect(de.guide.keyStoreWhy).toContain("nur auf diesem PC liegt, zählt nicht");
    expect(en.guide.keyStoreWhy).toContain("only lies on this computer does not count");
  });

  it("calls the token's secret 'Secret' in German, never 'Geheimnis'", () => {
    const text = JSON.stringify(de.guide);
    expect(text).not.toMatch(/Geheimnis/);
    expect(de.guide.setupIntro).toContain("Secret");
  });
});
