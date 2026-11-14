// A computer that joined a hub (decision 0034): the household master key stands in for the
// confirmed copy of its own key; `hub` names the hub. Both fields may be absent (older daemon).
import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { backupBlocked, keyCopyMissing, masterKeyProtected, selfKind, setupGuide } from "../src/lib/derive";
import { createFormatter } from "../src/lib/format";
import { encryptionFact, selfRowText } from "../src/lib/text";
import { World } from "../mock/world";
import type { Status } from "../src/lib/types";

const f = createFormatter("de", "de-DE", "Europe/Berlin");
const now = new Date("2026-11-08T18:00:00Z");
const master = { fingerprint: "<FINGERPRINT>", proven_at: "2026-11-01T10:00:00Z" };

function status(over: Partial<Status> = {}, enc: Partial<Status["encryption"]> = {}): Status {
  return {
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups", credentials_stored: true },
    schedule: { enabled: true },
    encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: false, key_present: true, ...enc },
    health: "never",
    ...over,
  };
}

describe("household master key", () => {
  it("without the field nothing changes: an unconfirmed copy still blocks", () => {
    const s = status();
    expect(masterKeyProtected(s)).toBe(false);
    expect(keyCopyMissing(s)).toBe(true);
    expect(selfKind(s)).toBe("key_missing");
    expect(backupBlocked(s)).toBe("key_confirm");
    expect(setupGuide(s)?.current).toBe("key_store");
    expect(encryptionFact(s, de)).toEqual({ text: de.self.encryptionOnNoKey, warn: true, extra: null });
  });

  it("null means no master key", () => {
    const s = status({}, { master_key: null });
    expect(keyCopyMissing(s)).toBe(true);
    expect(setupGuide(s)?.current).toBe("key_store");
  });

  it("with a master key the key step counts as done and nothing is blocked", () => {
    const s = status({}, { master_key: master });
    expect(keyCopyMissing(s)).toBe(false);
    expect(selfKind(s)).toBe("never");
    expect(backupBlocked(s)).toBeNull();
    // The step stays in the list (shown as done with one line), the next open step is current.
    expect(setupGuide(s)).toEqual({ steps: ["connect", "credentials", "key_create", "key_store", "automatic", "first_backup"], current: "first_backup" });
    expect(selfRowText(s, de, f, now)).toBe(de.row.never);
  });

  it("the own copy is an optional extra, never a warning", () => {
    expect(encryptionFact(status({}, { master_key: master }), en)).toEqual({ text: en.self.encryptionMaster, warn: false, extra: en.self.encryptionMasterOwn });
    expect(encryptionFact(status({}, { master_key: master, key_exported: true }), en)).toEqual({ text: en.self.encryptionMaster, warn: false, extra: null });
    expect(encryptionFact(status({}, { enabled: false, master_key: master }), en).text).toBe(en.self.encryptionOff);
  });

  it("the mock fault `hub` reports both fields and lets backups run", () => {
    const w = new World("ok", "full");
    w.faults.add("hub");
    const s = w.handle("GET", "/status", new URLSearchParams(), null).body as Status;
    expect(s.hub).toEqual({ name: "Home", url: "https://hub.example.com:8443" });
    expect(s.encryption.key_exported).toBe(false);
    expect(s.encryption.master_key?.fingerprint).toBe("<FINGERPRINT>");
    expect(selfKind(s)).toBe("ok");
    const plain = new World("ok", "full").handle("GET", "/status", new URLSearchParams(), null).body as Status;
    expect(plain.hub).toBeUndefined();
    expect(plain.encryption.master_key).toBeUndefined();
  });
});
