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
    copied: "Copied",
    copyFailed: "Could not copy, please select it",
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
      "Setup happens in a terminal. This command shows what it needs (server, datastore, access). Afterwards this page shows when the last backup happened.",
    keyLead: "Backups are waiting for the key copy to be confirmed.",
    keyText:
      "nestegg only backs up this computer once you have confirmed that a copy of the encryption key is stored outside of it. It takes a few minutes in a terminal, see below.",
    lastSuccessSentence: (m: Moment) => `Last backed up ${at(m)}.`,
    noSuccessYet: "There is no successful backup yet.",
    okLead: (m: Moment) => `Backed up ${at(m)}.`,
    okLeadUndated: "Backed up.",
    okText: "Everything is fine.",
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
    nextOff: "None, automatic backups are off",
    onServer: (size: string, count: number) =>
      `${size} in the latest backup, ${count} ${plural(count, "backup", "backups")} in total`,
    onServerCount: (count: number) => `${count} ${plural(count, "backup", "backups")}`,
    factEncryption: "Encryption",
    encryptionOnKey: "On. A copy of the key is confirmed.",
    encryptionOnNoKey: "On. A copy of the key is not confirmed yet.",
    encryptionOff: "Off. The backups are stored unencrypted on the server.",
  },
  key: {
    title: "A copy of the key has not been confirmed yet",
    text:
      "The backups of this computer are encrypted. No copy of the key outside this computer has been confirmed yet. If you have not stored one, a hardware failure could make the backups impossible to open. That is why nestegg only backs up once you have confirmed a copy.",
    steps: "In a terminal, one after the other:",
    paperkey: "Print the key as a paper copy and keep it in a safe place.",
    confirm: "Then confirm that the copy is stored safely.",
    otherTitle: "No copy of the key confirmed",
    otherText: (name: string) =>
      `The backups of this computer are encrypted. For ${name}, no copy of the key outside the computer has been confirmed. If there is none, a hardware failure of ${name} could make its backups impossible to open.`,
    otherFix: (name: string) => `This can only be fixed on ${name} itself, with these commands in a terminal:`,
  },
  strip: {
    title: "Last 14 days",
    ok: "Backed up",
    fail: "Failed",
    pbs: "Server not reachable",
    none: "No backup",
    run: "Running now",
  },
  runs: {
    succeeded: "Backed up",
    succeededManual: "Backed up, started by hand",
    succeededWarnings: (n: number) => `Backed up, with ${n} ${plural(n, "note", "notes")}`,
    failed: (reason: string) => `Failed: ${reason}`,
    pbs: "Server not reachable",
    pbsTimes: (n: number) => `Server not reachable, ${n} attempts`,
    cancelled: "Cancelled",
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
  },
  settings: {
    title: "Settings",
    intro: "Changes are saved immediately and apply from the next backup.",
    introReadOnly:
      "This nestegg version only shows the settings. Change them in a terminal with sudo nestegg init or in the file /etc/nestegg/config.toml.",
    offline: "Nothing can be changed without a connection to the nestegg service.",
    unconfigured: "Not set up yet. This command shows what is needed:",
    schedule: "Schedule",
    automatic: "Back up automatically",
    automaticOn: "nestegg checks every hour whether a backup is due and catches up on missed ones.",
    automaticOff: "Backups only run when you start them yourself.",
    interval: "Rhythm",
    intervalHint: "How often a backup is due.",
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
      overdueNotGreater: (interval: number) =>
        `Must be longer than the rhythm (${interval} hours). Otherwise every backup counts as overdue before the next one is due.`,
      excludeEmpty: "Please enter a pattern, for example **/.cache.",
      excludeTooLong: (n: number) => `A pattern may have at most ${n} characters.`,
      excludeControl: "The pattern contains a line break or another control character.",
      excludeSpaces: "The pattern must not start or end with a space.",
      excludeDash: "The pattern must not start with a minus sign.",
      excludeEverything: "Nothing would be backed up any more. Please choose a narrower pattern.",
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
    oneArea: (area: string) => `One restore takes files from one area. Start a second one for ${area} afterwards.`,
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
