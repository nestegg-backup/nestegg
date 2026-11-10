// English UI strings. Same keys as de.ts (checked by the type system and by
// tests/i18n.test.ts).

import type { Moment } from "../lib/format";
import type { Messages } from "./de";

const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/** "today at 01:42", "yesterday at 01:42", "on Sunday 8 November at 01:42" */
const at = (m: Moment) =>
  m.rel === "today"
    ? `today at ${m.time}`
    : m.rel === "yesterday"
      ? `yesterday at ${m.time}`
      : m.rel === "tomorrow"
        ? `tomorrow at ${m.time}`
        : `on ${m.day} at ${m.time}`;

const plural = (n: number, one: string, many: string) => (n === 1 ? one : many);

const en = {
  at,
  duration: {
    seconds: (n: number) => `${n} s`,
    minutes: (n: number) => `${n} min`,
    hours: (h: number) => `${h} h`,
    hoursMinutes: (h: number, m: number) => `${h} h ${m} min`,
  },
  app: {
    title: (page: string) => `${page} – nestegg`,
    loading: "Loading …",
    retry: "Try again",
    copy: "Copy command",
    copyOf: (command: string) => `Copy command: ${command}`,
    copied: "Copied",
    copyFailed: "Could not copy, please select it",
    copyMissing: "Not complete yet.",
    version: (v: string) => `nestegg ${v}`,
  },
  theme: {
    label: "Appearance",
    system: "System",
    light: "Light",
    dark: "Dark",
  },
  title: {
    selfSub: (os: string | null) => (os ? `this computer, ${os}` : "this computer"),
    otherSub: "view only",
  },
  sbar: {
    serverReachable: (m: Moment, dayMonth: string) =>
      m.rel === "today" ? `Backup server reachable, checked today at ${m.time}` : `Backup server reachable, checked on ${dayMonth}`,
    serverUnreachable: (m: Moment, dayMonth: string) =>
      m.rel === "today"
        ? `Backup server not reachable, checked today at ${m.time}`
        : `Backup server not reachable, checked on ${dayMonth}`,
    serverUnknown: "Backup server not checked yet",
    offline: "No connection to the nestegg service",
    sessionEnded: "Session ended",
    saved: (time: string, what: string) => `Saved at ${time}: ${what}`,
    notSaved: (what: string) => `Not saved: ${what}`,
  },
  errors: {
    daemonTitle: "No connection to the nestegg service",
    daemonText:
      "The interface is running, but the background service does not answer. This command in a terminal shows whether it is running:",
    authTitle: "The session has ended",
    authText:
      "Open the address that nestegg ui printed in the terminal. If nestegg ui is no longer running, start it again; it prints a new address.",
    genericTitle: "Something went wrong",
    genericText: "The answer of the nestegg service could not be processed.",
    detail: (msg: string) => `Message: ${msg}`,
    notImplemented: "This nestegg version cannot do that yet.",
  },
  conn: {
    lost: "No connection to the nestegg service.",
    stale: (m: Moment) =>
      `The information is from ${at(m)} and may be out of date. The page keeps asking and updates by itself.`,
    authLost: "The session has ended. Open the address that nestegg ui printed in the terminal, or start nestegg ui again.",
    runUnknown: "How far the backup has got is unknown right now.",
    restoreUnknown:
      "Whether the restore is still running is unknown right now: the nestegg service does not answer. The page keeps asking.",
  },
  load: {
    runsFailed: "The history could not be loaded.",
    configFailed: "The setup could not be loaded, so folders and exclusions are missing here.",
    notLoaded: "not loaded",
    loading: "loading …",
    snapshotsPbs: "Not available right now, the backup server does not answer.",
    snapshotsFailed: "Not available right now.",
    othersFailed: "Other computers are not available right now.",
  },
  nav: {
    label: "Computers in the household",
    thisPc: "This computer",
    others: "Other computers in the household",
    othersNote: "View only. Each of these computers is managed on the computer itself.",
    server: "Backup server",
    settings: "Settings",
    allPcs: "All computers",
  },
  row: {
    running: "Backing up now",
    unconfigured: "Not set up yet",
    keyMissing: "Key copy not confirmed",
    keyCreate: "Key still missing",
    credentials: "Token secret missing",
    incompleteUndated: "Backed up, incomplete",
    incomplete: (m: Moment, dayMonth: string) =>
      m.rel === "today"
        ? `Backed up today ${m.time}, incomplete`
        : m.rel === "yesterday"
          ? `Backed up yesterday ${m.time}, incomplete`
          : `Backed up ${dayMonth} ${m.time}, incomplete`,
    failing: (day: string) => `Failed on ${day}`,
    failingUndated: "Last backup failed",
    overdue: (day: string) => `Overdue, last ${day}`,
    never: "Never backed up",
    neverServer: "Earlier backups on the server",
    overdueUndated: "Overdue",
    okUndated: "Backed up",
    ok: (m: Moment, dayMonth: string) =>
      m.rel === "today"
        ? `Backed up today ${m.time}`
        : m.rel === "yesterday"
          ? `Backed up yesterday ${m.time}`
          : `Backed up ${dayMonth} ${m.time}`,
    serverReachable: (time: string) => `Reachable, checked at ${time}`,
    serverReachableDay: (day: string) => `Reachable, checked on ${day}`,
    serverUnreachable: (time: string) => `Not reachable, checked at ${time}`,
    serverUnreachableDay: (day: string) => `Not reachable, checked on ${day}`,
    serverUnknown: "Not checked yet",
  },
  self: {
    runNow: "Back up now",
    starting: "Starting …",
    restore: "Restore files",
    runConflict: "A backup is already running.",
    runStartFailed: (msg: string) => `The backup could not be started. ${msg}`,
    unconfiguredLead: "nestegg is not set up on this computer yet.",
    unconfiguredText:
      "Setup happens in a terminal. Below, you find step by step what to enter; the page moves on by itself as soon as a step is done.",
    keyLead: "Backups are waiting for the key copy to be confirmed.",
    keyText:
      "nestegg only backs up this computer once you have confirmed that a copy of the encryption key is stored outside of it. It takes a few minutes in a terminal, see below.",
    lastSuccessSentence: (m: Moment) => `Last backed up ${at(m)}.`,
    noSuccessYet: "There is no successful backup yet.",
    okLead: (m: Moment) => `Backed up ${at(m)}.`,
    okLeadUndated: "Backed up.",
    okText: "Everything is fine.",
    okTextDamaged: "The last backup worked. The server did report damaged backups though, see below.",
    nextRun: (m: Moment) =>
      `The next backup is due ${at(m)}. If the computer is off then, it catches up the next time it is switched on.`,
    nextRunDue: "The next backup is due and starts shortly.",
    scheduleOff: "Automatic backups are switched off. Backups only happen when you start them.",
    lastAttemptPbs: (m: Moment) => `The last attempt ${at(m)} could not reach the backup server.`,
    autoRetry: "nestegg tries again every hour by itself.",
    failingLead: (m: Moment) => `The backup ${at(m)} failed.`,
    failingLeadUndated: "The last backup failed.",
    overdueLead: (days: number, hours: number) =>
      days >= 2 ? `Not backed up for ${days} days.` : `Not backed up for ${hours} hours.`,
    overduePbs: "The backup server was not reachable during the last attempts. This computer did nothing wrong.",
    overdueCatchUp: "As long as the computer is on, nestegg catches up by itself.",
    neverLead: "No backup yet.",
    neverLeadServer: "No backup from this installation yet.",
    neverServer: (count: number, m: Moment) =>
      `The server holds ${count} earlier ${plural(count, "backup", "backups")} of this computer. The newest was made ${at(m)}. You can restore files from ${count === 1 ? "it" : "them"}.`,
    neverNext: (m: Moment) => `The first backup is due ${at(m)}. You can also start it now.`,
    neverNow: "You can start the first backup now.",
    neverAttempt: (m: Moment) => `The last attempt ${at(m)} did not work.`,
    runningLead: "Backup in progress.",
    runningText: "You can close this page; the backup keeps running in the background.",
    runningSince: (dur: string, manual: boolean) => `${manual ? "Started by hand" : "Started by the schedule"}, running for ${dur}`,
    stepWake: "Wake the server",
    stepReach: "Reach the server",
    stepWakeNow: "Waking the server. Waiting for an answer.",
    stepReachNow: "Connecting to the server.",
    stepStartingNow: "Starting the backup.",
    stepWakeDone: "Server is reachable",
    stepInventory: "List programs and drives",
    stepInventorySub: "Package lists, services, drives",
    stepUpload: "Upload changed files",
    stepUploadSub: (size: string) => `${size} uploaded`,
    stepUploadCheck: "Comparing files",
    stepUploadProgress: (done: string, total: string) => `${done} of about ${total} compared`,
    stepFinish: "Finish",
    stepFinishSub: "Send the report to the server",
    factsTitle: "About this computer",
    factBackupId: "Name on the server",
    factOs: "System",
    factSources: "Backed up",
    factSourcesExtra: "and a list of installed programs, services and drives",
    factOnServer: "On the server",
    factNext: "Next backup",
    nextAt: (m: Moment) => cap(at(m)),
    nextDue: "Due, starts shortly",
    nextBlocked: {
      credentials: "None yet, the token secret is missing",
      key_create: "None yet, there is no key",
      key_confirm: "None yet, the key copy is not confirmed",
      credentials_unreadable: "None yet, the stored secrets cannot be read",
    },
    nextOff: "None, automatic backups are off",
    onServer: (size: string, count: number) =>
      `${size} in the latest backup, ${count} ${plural(count, "backup", "backups")} in total`,
    onServerCount: (count: number) => `${count} ${plural(count, "backup", "backups")}`,
    factEncryption: "Encryption",
    encryptionOnKey: "On. A copy of the key is confirmed.",
    encryptionOnNoKey: "On. A copy of the key is not confirmed yet.",
    encryptionOff: "Off. The backups are stored unencrypted on the server.",
    incompleteLead: (m: Moment) => `Backed up ${at(m)}, but not completely.`,
    incompleteLeadUndated: "Backed up, but not completely.",
    incompleteText: "The backup worked, but it does not contain everything that is set up. What is missing is listed below.",
    incompleteTextNoList:
      "The backup worked, but it does not contain everything that is set up. nestegg did not report what is missing; the log of the backup in the history may show it.",
    incompleteTitle: "What is missing from the last backup",
    incompleteNote: "nestegg reports the reasons in English.",
    credentialsLead: "The secret of the token is still missing.",
    credentialsText:
      "The setup exists, but nestegg cannot log in to the backup server yet: the secret of the API token is not stored on this computer. Backups only start after that. How to do it is shown below.",
    keyCreateLead: "There is no key for the backups yet.",
    keyCreateText:
      "nestegg encrypts the backups before they leave this computer. That needs a key, which you create below. Backups only start after that.",
    lastCancelled: (m: Moment) => `The last backup was cancelled ${at(m)}.`,
    neverCancelled: (m: Moment) => `The last attempt ${at(m)} was cancelled.`,
    cancel: "Cancel backup",
    cancelAsk: "Really cancel the backup?",
    cancelAskText: "Data that was already uploaded is not lost; the next backup can build on it.",
    cancelYes: "Yes, cancel",
    cancelNo: "Keep backing up",
    cancelling: "Cancelling …",
    cancelGone:
      "Cancelling did not work: the backup is no longer running, or it is only just starting or already finishing. If it is still running, try again in a few seconds.",
    cancelCannot:
      "Cancelling is not possible right now: the backup is only just starting, or it has already finished. If it is still running, try again in a few seconds.",
    cancelFailed: (msg: string) => `Cancelling did not work. ${msg}`,
    factInterval: "Rhythm",
    factVerified: "Check",
    verifiedYes: "Read back from the server and checked after the upload",
    damagedTitle: "The server found damaged backups",
    damagedText: (n: number) =>
      `In its own check, the backup server reported ${n === 1 ? "one backup" : `${n} backups`} of this computer as damaged:`,
    damagedDay: (day: string, time: string) => `Backup from ${day}, ${time}`,
    damagedMore:
      "Newer and other backups are not affected. When restoring, the damaged ones are marked; pick another one then. It is best to start a new backup. If the server reports this again, have a look at its disks and free space.",
  },
  guide: {
    title: "The next steps",
    stepsLabel: "Steps to the first backup",
    done: "done",
    now: "current step",
    later: "later",
    connect: "Connect to the backup server",
    credentials: "Store the token secret",
    keyCreate: "Create the key",
    keyStore: "Store the key safely",
    automatic: "Switch on automatic backups",
    firstBackup: "First backup",
    moveOn: "As soon as a step is done, this page moves on to the next one by itself.",
    terminal: "You type the commands in a terminal. The button next to a command copies it.",
    setupIntro:
      "nestegg needs three things from the backup server: its address, the datastore (the storage space there) and an API token, an access key for this computer only. A token consists of an ID and a secret (the web interface of the server calls it “Secret”). You enter the ID below; you never type the secret on this page, only in the terminal in the next step.",
    setupClient:
      "The program proxmox-backup-client must be installed as well. How to do that is described in the documentation (docs/configuration.md, section “Installing the backup client”).",
    server: "Address of the backup server",
    serverPlaceholder: "pbs.example.com:8007",
    datastore: "Datastore",
    datastorePlaceholder: "backups",
    namespace: "Namespace (may stay empty)",
    namespacePlaceholder: "desktops",
    authId: "Token ID",
    authIdHint: "Format: user@realm!tokenname. Only the ID, not the secret.",
    authIdPlaceholder: "backup@pbs!desktop",
    fieldsNote: "These entries stay on this page and are not stored anywhere. They complete the command below.",
    commandsIntro: "Then, in the terminal:",
    cmdInit: "Creates the setup.",
    fillFields: "Fill in the fields above first.",
    fingerprint:
      "If the server uses a self-generated certificate, add --fingerprint=<SHA-256> to the command. The server shows the value in its dashboard under “Show Fingerprint”.",
    groupNote:
      "You are already allowed to open this page. Other users of this computer first have to join the group nestegg: sudo usermod -aG nestegg <NAME>, with their user name instead of <NAME>. After that they log out and in once.",
    credentialsWhy:
      "The setup exists, but the secret of the token is not stored on this computer yet. The command asks for it; what you type stays invisible. Then nestegg checks that server, access and backup program are fine.",
    cmdPassword: "Asks for the secret of the token.",
    cmdCheck: "Checks that server, access and backup program are fine.",
    cmdCheckNote:
      "One line stays red for now: “backup timer enabled” shows FAIL until you switch on automatic backups in a later step. That is expected. If the backup server is asleep, its lines show skip (not checked) instead; sudo nestegg check --wake wakes it first.",
    automaticWhy:
      "With automatic backups on, nestegg checks every hour whether a backup is due and catches up on missed ones. You can switch them on here or in a terminal.",
    automaticButton: "Switch on automatic backups",
    automaticWorking: "Switching on …",
    automaticDone: "Automatic backups are on.",
    automaticFailed: (reason: string) => `Switching on did not work. ${reason}`,
    automaticOr: "Or in a terminal:",
    keyCreateWhy:
      "The backups are encrypted on this computer before they go to the server. You create the key for that now.",
    keyCreateImport:
      "If you already have a key from before, for example after a reinstall, do not create a new one but bring the old one back: sudo nestegg key import <key file>. Otherwise the older backups cannot be opened.",
    keyStoreWhy:
      "Without this key nobody can open the backups, not even you if the computer breaks. That is why a copy must be kept outside this computer. A copy that only lies on this computer does not count, nor does one inside a folder that is backed up, even on an external disk. There are two ways; one is enough. Backups only start once you have confirmed the copy.",
    usbTitle: "On a USB stick",
    usbText:
      "Write the key to a USB stick and keep the stick somewhere other than the computer. Enter where the stick is mounted, usually below /media.",
    stickLabel: "Path of the USB stick",
    stickPlaceholder: "/media/alice/STICK",
    stickHint: "The full path, starting with /. The file nestegg-key.json is created there.",
    stickInvalid: "This must be a full path starting with /.",
    fillStick: "Enter the path of the stick first.",
    usbConfirm: "Then confirm that the file is on the stick:",
    usbUnplug: "When nestegg has confirmed it, unplug the stick and keep it away from the computer.",
    usbLocal:
      "If the file lies on a disk of this computer instead, nestegg refuses, because a copy that is lost with the computer is no protection. Only if another copy exists elsewhere, add --accept-local-copy to the second command.",
    paperTitle: "On paper",
    paperText:
      "Creates a page to print, with the key as a QR code and a short check code. Do these four steps in this order.",
    paperWrite: "Write the paper key file. It is named nestegg-paper-key.html and lies in your home folder:",
    paperPrint: "Open the file in the browser and print it. Keep the printout safe, not next to the computer.",
    paperDelete:
      "Delete the file. As long as it still lies on this computer, nestegg refuses to confirm:",
    paperConfirm: "Confirm with the check code at the top of the printed page:",
    paperAsk:
      "nestegg then asks whether the sheet is printed and stored away from the computer. Type yes if it is.",
    codeLabel: "Check code of the paper key",
    codePlaceholder: "ABCD-0123",
    codeHint: "Not secret. Eight characters from 0–9 and A–F, with or without hyphen.",
    codeInvalid: "That does not look like a check code (eight characters from 0–9 and A–F).",
    fillCode: "Enter the check code first.",
    firstBackupText:
      "Everything is ready. Click “Back up now” above. The first time, everything is uploaded, which can take a long time. You can close the page meanwhile.",
  },
  key: {
    otherTitle: "No copy of the key confirmed",
    otherText: (name: string) =>
      `The backups of this computer are encrypted. For ${name}, no copy of the key outside the computer has been confirmed. If there is none, a hardware failure of ${name} could make its backups impossible to open.`,
    otherFix: (name: string) =>
      `This can only be fixed on ${name} itself. Open nestegg there in the browser: the page guides you step by step through storing the key safely.`,
  },
  strip: {
    title: "Last 14 days",
    ok: "Backed up",
    fail: "Failed",
    pbs: "Server not reachable",
    none: "No backup",
    run: "Running now",
    incomplete: "Backed up incompletely",
    damaged: "Damaged on the server",
  },
  runs: {
    succeeded: "Backed up",
    succeededManual: "Backed up, started by hand",
    succeededWarnings: (n: number) => `Backed up, with ${n} ${plural(n, "note", "notes")}`,
    failed: (reason: string) => `Failed: ${reason}`,
    pbs: "Server not reachable",
    pbsTimes: (n: number) => `Server not reachable, ${n} attempts`,
    cancelled: "Cancelled",
    succeededIncomplete: "Backed up, but incomplete",
    damaged: "Backed up, but damaged on the server",
    incompleteReasons: "What is missing",
    title: "History",
    colDay: "Day",
    colStart: "Start",
    colResult: "Result",
    colDuration: "Duration",
    colUploaded: "Uploaded",
    showLog: "Log",
    hideLog: "Hide log",
    logLoading: "Loading log …",
    logEmpty: "There is no log for this run.",
    logFailed: "The log could not be loaded.",
    warnings: "Notes",
    showAll: (n: number) => `Show all ${n} entries`,
    showFewer: "Show fewer",
    empty: "No backups yet.",
    emptyHere: "No backup from this installation yet. The earlier ones are on the server.",
  },
  reason: {
    pbs_unreachable: "server not reachable",
    authentication: "access denied",
    source_missing: "folder missing",
    client_missing: "backup program missing",
    client_failed: "backup program reported an error",
    already_running: "already running",
    config: "setup incomplete",
    credentials_unreadable: "stored secrets unreadable",
    verification_failed: "check after the upload failed",
    cancelled: "cancelled",
    internal: "internal error",
  },
  explain: {
    pbs_unreachable:
      "The backup server did not answer. This computer did nothing wrong.",
    authentication:
      "The backup server refused access. Most likely the API token was changed or deleted. Store it again with sudo nestegg credentials.",
    source_missing:
      "A folder that should be backed up was missing, for example because a disk was not mounted. nestegg stops in that case instead of reporting an incomplete backup as successful.",
    client_missing: "The program proxmox-backup-client is not installed. nestegg needs it for every backup.",
    client_failed: "The backup program stopped with an error. The log shows what exactly happened.",
    already_running: "Another backup was already running at that time. Nothing was lost.",
    config: "The setup is incomplete. The log shows what is missing; sudo nestegg check shows it too.",
    credentials_unreadable:
      "This computer can no longer decrypt the stored token secret or key, for example after a mainboard change or a restore onto other hardware. Store them again with sudo nestegg credentials set-password and sudo nestegg key import followed by your exported key file.",
    verification_failed:
      "The backup was uploaded, but reading it back from the server returned something different from what was sent, or nothing at all. That is why it does not count as successful. Start the backup again. If the error repeats, check the backup server: free space, disks, network.",
    cancelled: "The backup was cancelled. Data that was already uploaded is not lost.",
    internal: "An error happened inside nestegg itself. The log shows what exactly happened.",
  },
  other: {
    failingLead: (m: Moment) => `The backup ${at(m)} failed.`,
    overdueLead: (days: number, hours: number) =>
      days >= 2 ? `Not backed up for ${days} days.` : `Not backed up for ${hours} hours.`,
    overdueText: (m: Moment) => `Last backed up ${at(m)}. No newer backup has arrived on the server since.`,
    neverLead: "The server has no backup of this computer yet.",
    neverText: "Once it has made its first backup, more will show up here.",
    okLead: (m: Moment) => `${cap(`backed up ${at(m)}`)}.`,
    okText: "Everything is fine, as far as the backup server can tell.",
    pbsText: "The backup server was not reachable during the last attempt.",
    lastSuccess: (m: Moment) => `Last successful backup ${at(m)}.`,
    reportedError: "Reported error",
    factsTitle: "What the server knows about this computer",
    factKey: "Copy of the key confirmed",
    yes: "Yes",
    no: "No",
    unknown: "Unknown",
    factRead: "As of",
    readAt: (m: Moment) => `Read from the server ${at(m)}`,
    factVersion: "nestegg version",
    notFound: "This computer is not (or no longer) in the list.",
  },
  server: {
    reachableLead: "Reachable at the last check.",
    unreachableLead: "Not reachable at the last check.",
    unknownLead: "Not checked yet.",
    checked: (m: Moment) => `Checked ${at(m)}.`,
    unknownText: "nestegg checks the connection with every backup.",
    contactsTitle: "During the last backups",
    contactsText: "How the server responded to the last backups of this computer.",
    contactOk: "Reached",
    contactPbs: "Not reached, no backup",
    contactPbsTimes: (n: number) => `Not reached, ${n} attempts`,
    contactUnknown: "Not checked, the run ended earlier",
    contactsEmpty: "No backup attempted yet.",
    storageTitle: "Storage",
    address: "Address",
    datastore: "Datastore",
    namespace: "Namespace",
    noNamespace: "none",
    wol: "Wake-on-LAN",
    on: "On",
    off: "Off",
    retention: "Retention",
    retentionText: "Configured on the server.",
    usage: "Used",
    usageOf: (used: string, total: string) => `${used} of ${total}`,
    check: "Check connection",
    checking: "Checking …",
    checkingText: "nestegg checks whether the backup server answers and the access works.",
    checkingWake:
      "The backup server is woken if needed (Wake-on-LAN). That can take a few minutes. You can leave the page; the result will be here afterwards.",
    checkTitle: "Result of the check",
    checkedAt: (m: Moment) => `Checked ${at(m)}.`,
    checkAllOk: "Everything is fine.",
    checkProblems: (n: number) => `${n} ${plural(n, "item has", "items have")} a problem.`,
    checkNames: "The names and messages come from the program and are in English.",
    checkOkLabel: "fine",
    checkBadLabel: "problem",
    checkPbs: "The backup server did not answer. Is it switched on?",
    checkPbsWol: "Wake-on-LAN is switched on, so nestegg tried to wake it. Try again later.",
    checkIncomplete: "Not everything could be checked.",
    checkSkipped: (n: number) => `${n} not checked.`,
    checkNotCheckedLabel: "not checked",
    checkAsleep:
      "The backup server did not answer. It may be asleep. Whether the login and the access to the datastore work was not checked.",
    checkWakeAgain: "Wake the server and check again",
    checkRunning: "Running: checking the connection.",
    checkFinished: (problems: number, skipped = 0) =>
      problems
        ? `Check finished, ${problems} ${plural(problems, "item has", "items have")} a problem.`
        : skipped
          ? "Check finished, but not everything could be checked."
          : "Check finished, everything is fine.",
    checkFailed: (msg: string) => `The check could not be run. ${msg}`,
  },
  settings: {
    title: "Settings",
    intro: "Changes are saved immediately and apply from the next backup.",
    reported:
      "Changes that reduce the protection are reported to everyone who uses this computer: automatic backups switched off, a new exclude, a longer rhythm or a longer time before “overdue”, Wake-on-LAN switched off.",
    introReadOnly:
      "This nestegg version only shows the settings. Change them in a terminal in the file /etc/nestegg/config.toml (sudoedit /etc/nestegg/config.toml).",
    offline: "Nothing can be changed without a connection to the nestegg service.",
    unconfigured: "Not set up yet. How to do that is described step by step on the page of the computer.",
    unconfiguredLink: "Go to the setup",
    schedule: "Schedule",
    automatic: "Back up automatically",
    automaticOn: "nestegg checks every hour whether a backup is due and catches up on missed ones.",
    automaticOff: "Backups only run when you start them yourself.",
    interval: "Rhythm",
    intervalHint: "How often a backup is due.",
    intervalLimit: (max: number) =>
      `Longer rhythms are not possible here: “overdue” may be at most ${max} hours here, and it must be longer than the rhythm. An administrator can set more in /etc/nestegg/config.toml.`,
    everyHours: (h: number) =>
      h === 1
        ? "Every hour"
        : h === 24
          ? "Daily"
          : h === 168
            ? "Weekly"
            : h % 24 === 0
              ? `Every ${h / 24} days`
              : `Every ${h} hours`,
    overdue: "Report as overdue after",
    overdueHint: "Saved when you leave the field or press Enter.",
    hours: "hours",
    excludes: "Excluded folders",
    excludesHint: (sources: string) => `Whatever matches these patterns is not backed up. Backed up are ${sources}.`,
    excludesHintPlain: "Whatever matches these patterns is not backed up.",
    excludesNone: "Nothing is excluded so far.",
    remove: "Remove",
    removeLabel: (pattern: string) => `Remove ${pattern}`,
    add: "Add",
    newPattern: "New pattern",
    placeholder: "for example **/Downloads",
    wake: "Wake the server",
    wol: "Wake-on-LAN",
    wolHint: "If the backup server is asleep, nestegg sends a wake-up signal before every backup and waits until it answers.",
    wolNotConfigured:
      "Not set up. nestegg needs the MAC address of the backup server for this. It is entered in a terminal (section [wake] in /etc/nestegg/config.toml); after that, Wake-on-LAN can be switched on and off here.",
    on: "On",
    off: "Off",
    fixed: "Server and encryption",
    fixedHint:
      "For reference only. Credentials and the key depend on these, so they are changed in a terminal with administrator rights.",
    changeIn: "Change in a terminal:",
    checkIn: "Check that access and server work:",
    address: "Server address",
    backupId: "Name on the server",
    encryption: "Encryption",
    fingerprint: "Key fingerprint",
    retention: "Retention",
    keyVerify: "To check later whether a copy of the key still fits, use nestegg key verify with the path of the copy.",
    retentionText: "Set on the server",
    saved: "Saved",
    saving: "Saving …",
    notSaved: (reason: string) => `Not saved. ${reason}`,
    sumAutomatic: (on: boolean) => `Automatic backups ${on ? "switched on" : "switched off"}`,
    sumInterval: (label: string) => `Rhythm: ${label.charAt(0).toLowerCase()}${label.slice(1)}`,
    sumIntervalOverdue: (label: string, hours: number) =>
      `Rhythm: ${label.charAt(0).toLowerCase()}${label.slice(1)}, report as overdue after ${hours} hours`,
    sumOverdue: (hours: number) => `Report as overdue after ${hours} hours`,
    sumExcludeAdded: (pattern: string) => `${pattern} is left out from now on`,
    sumExcludeRemoved: (pattern: string) => `${pattern} is backed up again`,
    sumWol: (on: boolean) => `Wake-on-LAN ${on ? "switched on" : "switched off"}`,
    overdueRaised: (hours: number) => `Raised to ${hours} hours because it must be longer than the rhythm.`,
    problem: {
      wholeNumber: "Please enter a whole number of hours.",
      intervalRange: (min: number, max: number) => `The rhythm must be between ${min} and ${max} hours.`,
      overdueRange: (min: number, max: number) => `Allowed are ${min} to ${max} hours.`,
      overdueCapped: (min: number, max: number, current: number | null) =>
        `Allowed are ${min} to ${max} hours${max % 24 === 0 ? ` (${max / 24} days)` : ""} here.${
          current === null ? "" : ` The current value of ${current} hours stands in the file like that and can only be shortened here, not made longer.`
        } A longer time can only be set by an administrator in /etc/nestegg/config.toml.`,
      overdueNotGreater: (interval: number) =>
        `Must be longer than the rhythm (${interval} hours). Otherwise every backup counts as overdue before the next one is due.`,
      excludeEmpty: "Please enter a pattern, for example **/.cache.",
      excludeTooLong: (n: number) => `A pattern may have at most ${n} characters.`,
      excludeControl: "The pattern contains a line break or another control character.",
      excludeSpaces: "The pattern must not start or end with a space.",
      excludeDash: "The pattern must not start with a minus sign.",
      excludeEverything: "Nothing would be backed up any more. Please choose a narrower pattern.",
      excludeSource: (what: string) =>
        `That would leave the whole backed-up folder “${what}” out of the backup. nestegg does not allow that here. An administrator can still set it in /etc/nestegg/config.toml.`,
      excludeTopLevel: (dir: string) =>
        `That would leave everything in “${dir}” out of every backup. nestegg does not allow that here. An administrator can still set it in /etc/nestegg/config.toml.`,
      excludeDuplicate: "This pattern is already in the list.",
      excludeTooMany: (n: number) => `More than ${n} patterns are not possible.`,
      wakeNotConfigured:
        "Wake-on-LAN is not set up: the MAC address of the backup server is missing. It is entered in a terminal.",
      terminalOnly: "This can only be changed in a terminal.",
      timerHalf:
        "Saved, but the schedule could not be started or stopped. Check it in the terminal with systemctl status nestegg-backup.timer.",
      daemon: "The nestegg service does not answer. Try again in a moment.",
      session: "The session has ended. Open the address that nestegg ui printed in the terminal.",
      forbidden: "nestegg could not tell who sent the change. Start nestegg ui again and retry.",
      timer: "Automatic backups could not be switched. Nothing was changed.",
      configBroken:
        "The configuration file is broken. Please repair it in a terminal first; sudo nestegg check shows what is wrong.",
      other: "The nestegg service refused the change.",
    },
  },
  restore: {
    title: "Restore files",
    cancel: "Cancel",
    stepOf: (n: number) => `Step ${n} of 3`,
    stepTitles: ["Choose a backup", "Choose files", "Choose the target folder"],
    pickIntro: "Which day should the files come from? Pick the last backup in which they were still fine.",
    newest: "Latest backups",
    older: (n: number, until: string) => `Show older backups (${n} more, back to ${until})`,
    loadingSnapshots: "Fetching backups from the server …",
    noSnapshots: "The server has no backup of this computer yet.",
    pbsUnreachable: "The backup server is not reachable right now. Restoring only works while it is running.",
    pbsUnreachableWol:
      "Wake-on-LAN is set up: a backup tries to wake it first. You can try again here afterwards.",
    showFiles: "Show the files of this backup",
    notAvailable:
      "This nestegg version cannot restore files from the interface yet. It already works in a terminal, for example for the latest backup:",
    notAvailableTarget: "<NEW-FOLDER>",
    snapshotOf: (day: string, time: string) => `Backup of ${day}, ${time}.`,
    otherSnapshot: "Choose another backup",
    inventory: "List of programs, services and drives",
    expand: (name: string) => `Expand ${name}`,
    collapse: (name: string) => `Collapse ${name}`,
    treeLabel: "Files in the backup",
    dirLoading: "Loading …",
    dirEmpty: "Empty folder",
    dirFailed: "This folder could not be opened.",
    manyAreas: (names: string) => `Each area goes into its own subfolder of the new folder (${names}).`,
    damaged: "damaged",
    damagedSnapshot:
      "This backup is marked as damaged on the server. Files from it may not be restorable completely. Better pick another one.",
    damagedAll:
      "All backups are marked as damaged on the server, this one too. Files from it may not be restorable completely. You can try anyway.",
    progress: (done: string, total: string) => `${done} of ${total} restored`,
    progressDone: (done: string) => `${done} restored`,
    nothingSelected: "Nothing selected yet",
    selected: (n: number, size: string | null) =>
      `${n} ${plural(n, "item", "items")} selected${size ? `, ${size} in total` : ""}`,
    toTarget: "Choose the target folder",
    targetIntro:
      "nestegg creates a new folder and writes only into it. Existing files are never overwritten. Afterwards you can pick out what you need at your own pace.",
    folderLabel: "Name of the new folder",
    pathLabel: "New folder (full path)",
    folderEmpty: "Please enter a name for the new folder.",
    folderSlash: "The name must not contain a slash.",
    folderInvalid: "This name does not work, please choose another one.",
    pathNotAbsolute: "Please enter a full path starting with /.",
    folderOk: (user: string) => `This folder must not exist yet. It will belong to you (${user}).`,
    pathOk: "This folder must not exist yet; nestegg creates it.",
    targetExists: "This folder already exists. nestegg only writes into a new folder, please choose another name.",
    summary: "Summary",
    sumSnapshot: "Backup",
    sumSelection: "Selection",
    sumSize: "Size",
    sumTarget: "Target folder",
    changeSelection: "Change selection",
    start: "Restore into the new folder",
    starting: "Starting …",
    running: "Bringing the files back.",
    runningTarget: "Target",
    runningSince: (dur: string) => `Running for ${dur}`,
    done: "Done. The files are now in",
    doneText: (n: number, day: string) =>
      `${n} ${plural(n, "item", "items")}, backup of ${day}. None of your other files were changed.`,
    failed: "Restoring failed.",
    backTo: (name: string) => `Back to ${name}`,
    tryAgain: "Back to the target folder",
    startFailed: (msg: string) => `The restore could not be started. ${msg}`,
  },
} satisfies Messages;

export default en;
