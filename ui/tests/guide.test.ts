// The setup and key guide: which step is current for which /status.
import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { initCommand, setupGuide, shellQuote } from "../src/lib/derive";
import { createFormatter } from "../src/lib/format";
import { selfRowText, selfSummary } from "../src/lib/text";
import type { Run, Status } from "../src/lib/types";

const f = createFormatter("de", "de-DE", "Europe/Berlin");
const now = new Date("2026-11-08T18:00:00Z");
const ok: Run = { id: "a", trigger: "timer", state: "succeeded", started_at: "2026-11-08T00:42:00Z" };

function status(over: Partial<Status> = {}): Status {
  return {
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups" },
    schedule: { enabled: true },
    encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: true },
    health: "ok",
    ...over,
  };
}

describe("setupGuide", () => {
  it("unconfigured: setup, with the key steps and the first backup still ahead", () => {
    const g = setupGuide(status({ health: "unconfigured", encryption: { enabled: false, key_fingerprint: null, key_exported: false } }));
    expect(g).toEqual({ steps: ["setup", "key_create", "key_store", "first_backup"], current: "setup" });
  });

  it("configured, encryption on, no key yet: create the key", () => {
    const g = setupGuide(status({ health: "never", encryption: { enabled: true, key_fingerprint: null, key_exported: false } }));
    expect(g?.current).toBe("key_create");
  });

  it("key exists, copy not confirmed: store it safely", () => {
    const g = setupGuide(status({ health: "never", encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: false } }));
    expect(g?.current).toBe("key_store");
    expect(g?.steps).toEqual(["setup", "key_create", "key_store", "first_backup"]);
  });

  it("key confirmed, no backup yet: first backup", () => {
    expect(setupGuide(status({ health: "never" }))?.current).toBe("first_backup");
  });

  it("moves on by itself: every /status gives the next step", () => {
    const order = [
      status({ health: "unconfigured" }),
      status({ health: "never", encryption: { enabled: true, key_fingerprint: null, key_exported: false } }),
      status({ health: "never", encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: false } }),
      status({ health: "never" }),
    ].map((s) => setupGuide(s)?.current);
    expect(order).toEqual(["setup", "key_create", "key_store", "first_backup"]);
    expect(setupGuide(status({ health: "ok", last_success: ok }))).toBeNull();
  });

  it("without encryption the key steps are left out", () => {
    const g = setupGuide(status({ health: "never", encryption: { enabled: false, key_fingerprint: null } }));
    expect(g).toEqual({ steps: ["setup", "first_backup"], current: "first_backup" });
  });

  it("a PC with backups but an unconfirmed key copy gets the key step without 'first backup'", () => {
    const g = setupGuide(
      status({ health: "failing", last_success: ok, encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: false } }),
    );
    expect(g).toEqual({ steps: ["setup", "key_create", "key_store"], current: "key_store" });
  });

  it("is not shown while a backup runs, and not for a healthy PC", () => {
    expect(setupGuide(status({ health: "running" }))).toBeNull();
    expect(setupGuide(status({ health: "never", current_run: { ...ok, state: "uploading" } }))).toBeNull();
    expect(setupGuide(status())).toBeNull();
  });

  it("the page says which step it is: no key at all differs from an unconfirmed copy", () => {
    const noKey = status({ health: "never", encryption: { enabled: true, key_fingerprint: null, key_exported: false } });
    expect(selfSummary(noKey, de, f, now).lead).toBe("Es gibt noch keinen Schlüssel für die Sicherungen.");
    expect(selfRowText(noKey, de, f, now)).toBe("Schlüssel fehlt noch");
    const unconfirmed = status({ health: "never", encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: false } });
    expect(selfSummary(unconfirmed, de, f, now).lead).toBe("Sicherungen warten auf die Bestätigung der Schlüsselkopie.");
    expect(selfRowText(unconfirmed, en, f, now)).toBe("Key copy not confirmed");
  });
});

describe("init command", () => {
  it("quotes what the shell would misread: the token ID contains !", () => {
    expect(shellQuote("backups")).toBe("backups");
    expect(shellQuote("pbs.example.com")).toBe("pbs.example.com");
    expect(shellQuote("backup@pbs!desktop")).toBe("'backup@pbs!desktop'");
    expect(shellQuote("it's here")).toBe("'it'\\''s here'");
    expect(shellQuote("a b")).toBe("'a b'");
    expect(shellQuote("$(rm -rf x)")).toBe("'$(rm -rf x)'");
  });

  it("composes the documented command", () => {
    expect(initCommand({ server: "pbs.example.com", datastore: "backups", namespace: "desktops", authId: "backup@pbs!desktop" })).toBe(
      "sudo nestegg init \\\n  --server pbs.example.com \\\n  --datastore backups \\\n  --auth-id 'backup@pbs!desktop' \\\n  --namespace desktops",
    );
  });

  it("leaves out an empty namespace and splits host:port", () => {
    const cmd = initCommand({ server: " pbs.example.com:8007 ", datastore: "backups", namespace: " ", authId: "backup@pbs!desktop" });
    expect(cmd).toContain("--server pbs.example.com \\\n  --port 8007 \\\n");
    expect(cmd).not.toContain("--namespace");
    expect(initCommand({ server: "[2001:db8::1]:8007", datastore: "b", namespace: "", authId: "a@b!c" })).toContain("--server 2001:db8::1 \\\n  --port 8007");
    // A bare IPv6 address (several colons, no brackets) is not mistaken for host:port.
    expect(initCommand({ server: "2001:db8::1", datastore: "b", namespace: "", authId: "a@b!c" })).toContain("--server 2001:db8::1 \\\n  --datastore");
  });

  it("shows placeholders for what is still missing, and never a secret option", () => {
    const cmd = initCommand({ server: "", datastore: "", namespace: "", authId: "" });
    expect(cmd).toBe("sudo nestegg init \\\n  --server <SERVER> \\\n  --datastore <DATASTORE> \\\n  --auth-id '<USER@REALM!TOKEN>'");
    expect(cmd).not.toMatch(/password|secret/i);
  });
});

describe("guide wording", () => {
  it("never asks for a secret on the page and says that a copy on this computer does not count", () => {
    expect(de.guide.setupIntro).toContain("nie hier auf der Seite");
    expect(en.guide.setupIntro).toContain("never type the secret");
    expect(de.guide.keyStoreWhy).toContain("nur auf diesem PC liegt, zählt nicht");
    expect(en.guide.keyStoreWhy).toContain("only lies on this computer does not count");
  });
});
