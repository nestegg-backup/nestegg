// "Uploaded" is the compressed amount actually sent (decision 0032), stick names replace bare UUIDs
// with the UUID as detail, and the mock serves both like the daemon.
import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { SECOND_STICK_UUID, STICK_UUID, World } from "../mock/world";
import { registeredSticks, shortUuid, stickName, transferredBytes } from "../src/lib/derive";
import { stickLabel } from "../src/lib/text";
import type { ConfigView, Run, Status, TargetStatus } from "../src/lib/types";

const params = new URLSearchParams();

describe("the uploaded amount", () => {
  it("is the compressed amount; older records have none and are not shown as uploaded", () => {
    expect(transferredBytes({ transferred_bytes: 780_903_907 })).toBe(780_903_907);
    expect(transferredBytes({ transferred_bytes: 0 })).toBe(0);
    expect(transferredBytes({})).toBeNull();
    expect(transferredBytes({ transferred_bytes: null })).toBeNull();
    // An old record with only the uncompressed new data: still nothing to show.
    const old = { uploaded_bytes: 2_066_953_011 } as Partial<Run>;
    expect(transferredBytes(old)).toBeNull();
  });

  it("the mock reports less sent than new data, and old records without it", () => {
    const w = new World("ok", "full");
    const runs = (w.handle("GET", "/runs", params, null).body as { runs: Run[] }).runs;
    const ok = runs.filter((r) => r.state === "succeeded");
    const sent = ok.filter((r) => r.transferred_bytes != null);
    expect(sent.length).toBeGreaterThan(0);
    for (const r of sent) expect(r.transferred_bytes!).toBeLessThan(r.uploaded_bytes!);
    // Records of an older version exist in the history and keep their old field.
    const legacy = ok.filter((r) => r.transferred_bytes == null);
    expect(legacy.length).toBeGreaterThan(0);
    for (const r of legacy) expect(r.uploaded_bytes).toBeGreaterThan(0);
  });

  it("the history column keeps its words in both languages", () => {
    expect(de.runs.colUploaded).toBe("Hochgeladen");
    expect(en.runs.colUploaded).toBe("Uploaded");
    expect(de.stick.colWritten).toBe("Geschrieben");
  });
});

describe("stick names", () => {
  const ts = (sticks: TargetStatus["sticks"]) => ({ sticks }) as TargetStatus;
  const cfg = (c: Partial<ConfigView>) => c as ConfigView;

  it("prefers the status name, then the config, case-insensitively", () => {
    const upper = STICK_UUID.toUpperCase();
    expect(stickName(upper, ts({ [STICK_UUID]: { plugged_in: true, in_use: null, name: "Blue" } }))).toBe("Blue");
    expect(stickName(STICK_UUID, ts({ [STICK_UUID]: { plugged_in: true, in_use: null } }), cfg({ stick_names: { [STICK_UUID]: "Office" } }))).toBe("Office");
    // Older daemons: no name anywhere.
    expect(stickName(STICK_UUID, ts({ [STICK_UUID]: { plugged_in: true, in_use: null } }), cfg({}))).toBeNull();
    expect(stickName(STICK_UUID, null, null)).toBeNull();
  });

  it("falls back to 'Stick' and the start of the UUID", () => {
    expect(shortUuid(STICK_UUID.toUpperCase())).toBe("01234567");
    expect(stickLabel(STICK_UUID, null, null, de)).toBe("Stick 01234567");
    expect(stickLabel(STICK_UUID, null, null, en)).toBe("Stick 01234567");
    expect(stickLabel(STICK_UUID, null, cfg({ stick_names: { [STICK_UUID]: "Blue stick" } }), en)).toBe("Blue stick");
  });

  it("never gives two unnamed sticks the same label", () => {
    const a = "01234567-89ab-cdef-0123-456789abcdef", b = "01234567-89ab-cdef-0123-456789abcdee";
    // They differ only in the last character: the whole UUID is needed.
    expect(shortUuid(a, [b])).toBe(a);
    expect(shortUuid(b, [a, b])).toBe(b);
    expect(shortUuid(a, ["01234567-8000-0000-0000-000000000000"])).toBe("01234567-89");
    expect(shortUuid(a, ["01234568-0000-0000-0000-000000000000"])).toBe("01234567");
    const config = cfg({ stick_data_uuids: [a, b] });
    expect(stickLabel(a, null, config, en)).not.toBe(stickLabel(b, null, config, en));
  });

  it("lists registered sticks once, in configuration order", () => {
    const list = registeredSticks(ts({ [SECOND_STICK_UUID]: { plugged_in: false, in_use: null } }), cfg({ stick_data_uuids: [STICK_UUID.toUpperCase(), SECOND_STICK_UUID] }));
    expect(list).toEqual([STICK_UUID, SECOND_STICK_UUID]);
  });

  it("the mock names the first stick in status and config and leaves the second unnamed", () => {
    const w = new World("both", "full");
    w.faults.add("stick_multiple");
    const stick = (w.handle("GET", "/status", params, null).body as Status).targets!.stick!;
    expect(stick.sticks![STICK_UUID]!.name).toBe("Blue stick");
    expect(stick.sticks![SECOND_STICK_UUID]!.name).toBeNull();
    const config = w.handle("GET", "/config", params, null).body as ConfigView;
    expect(config.stick_names).toEqual({ [STICK_UUID]: "Blue stick" });
    expect(config.stick_data_uuids).toEqual([STICK_UUID, SECOND_STICK_UUID]);
    expect(stickLabel(SECOND_STICK_UUID, stick, config, de)).toBe(`Stick ${SECOND_STICK_UUID.slice(0, 8)}`);
  });
});

describe("a computer that was restored from a backup", () => {
  it("the mock reports where it came from, and both languages say so", () => {
    const w = new World("ok", "full");
    expect((w.handle("GET", "/status", params, null).body as Status).restored_from ?? null).toBeNull();
    w.faults.add("restored");
    const from = (w.handle("GET", "/status", params, null).body as Status).restored_from!;
    expect(from.target).toBe("pbs");
    expect(new Date(from.restored_at).getTime()).toBeGreaterThan(new Date(from.snapshot_time).getTime());
    const when = { rel: "today", time: "10:00" } as never;
    const backup = { rel: "yesterday", time: "02:00" } as never;
    expect(de.self.restoredFrom(when, backup, false)).toBe(
      "Heute um 10:00, aus der Sicherung von gestern um 02:00 vom Backup-Server",
    );
    expect(en.self.restoredFrom(when, backup, true)).toBe(
      "Today at 10:00, from the backup made yesterday at 02:00 on the backup stick",
    );
  });
});
