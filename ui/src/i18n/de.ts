// German UI strings. The wording follows the chosen design prototype (variant C).
// en.ts must have exactly the same keys (checked by the type system and by
// tests/i18n.test.ts).

import type { Moment } from "../lib/format";
import type { RunErrorKind } from "../lib/types";

const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/** "heute um 01:42", "gestern um 01:42", "am Sonntag, 8. November, um 01:42" */
const at = (m: Moment) =>
  m.rel === "today"
    ? `heute um ${m.time}`
    : m.rel === "yesterday"
      ? `gestern um ${m.time}`
      : m.rel === "tomorrow"
        ? `morgen um ${m.time}`
        : `am ${m.day}, um ${m.time}`;

const plural = (n: number, one: string, many: string) => (n === 1 ? one : many);

const de = {
  at,
  duration: {
    seconds: (n: number) => `${n} Sek.`,
    minutes: (n: number) => `${n} Min.`,
    hours: (h: number) => `${h} Std.`,
    hoursMinutes: (h: number, m: number) => `${h} Std. ${m} Min.`,
  },
  app: {
    title: (page: string) => `${page} – nestegg`,
    loading: "Wird geladen …",
    retry: "Erneut versuchen",
    copy: "Befehl kopieren",
    copied: "Kopiert",
    copyFailed: "Kopieren ging nicht, bitte markieren",
  },
  errors: {
    daemonTitle: "Keine Verbindung zum nestegg-Dienst",
    daemonText:
      "Die Oberfläche läuft, aber der Dienst im Hintergrund antwortet nicht. Ob er läuft, zeigt dieser Befehl im Terminal:",
    authTitle: "Die Sitzung ist beendet",
    authText:
      "Öffne die Adresse, die nestegg ui im Terminal ausgegeben hat. Läuft nestegg ui nicht mehr, starte es neu; es gibt dann eine neue Adresse aus.",
    genericTitle: "Etwas ist schiefgelaufen",
    genericText: "Die Antwort des nestegg-Dienstes ließ sich nicht verarbeiten.",
    detail: (msg: string) => `Meldung: ${msg}`,
    notImplemented: "Das kann diese nestegg-Version noch nicht.",
  },
  conn: {
    lost: "Keine Verbindung zum nestegg-Dienst.",
    stale: (m: Moment) =>
      `Die Angaben stammen von ${at(m)} und sind vielleicht nicht mehr aktuell. Die Seite fragt weiter nach und aktualisiert sich von selbst.`,
    authLost:
      "Die Sitzung ist beendet. Öffne die Adresse, die nestegg ui im Terminal ausgegeben hat, oder starte nestegg ui neu.",
    runUnknown: "Wie weit die Sicherung ist, ist gerade unbekannt.",
    restoreUnknown:
      "Ob das Wiederherstellen noch läuft, ist gerade unbekannt: Der nestegg-Dienst antwortet nicht. Die Seite fragt weiter nach.",
  },
  load: {
    runsFailed: "Der Verlauf ließ sich nicht laden.",
    configFailed: "Die Einrichtung ließ sich nicht laden. Ordner und Ausnahmen fehlen deshalb hier.",
    notLoaded: "nicht geladen",
    loading: "wird geladen …",
    snapshotsPbs: "Gerade nicht abrufbar, der Backup-Server antwortet nicht.",
    snapshotsFailed: "Gerade nicht abrufbar.",
    othersFailed: "Andere PCs sind gerade nicht abrufbar.",
  },
  nav: {
    label: "PCs im Haushalt",
    thisPc: "Dieser PC",
    others: "Andere PCs im Haushalt",
    othersNote: "Nur zur Ansicht. Diese PCs werden jeweils am PC selbst verwaltet.",
    server: "Backup-Server",
    settings: "Einstellungen",
    allPcs: "Alle PCs",
    back: "Zurück",
  },
  row: {
    running: "Sichert gerade",
    unconfigured: "Noch nicht eingerichtet",
    keyMissing: "Schlüsselkopie nicht bestätigt",
    failing: (day: string) => `Fehlgeschlagen am ${day}`,
    failingUndated: "Letzte Sicherung fehlgeschlagen",
    overdue: (day: string) => `Überfällig, zuletzt ${day}`,
    never: "Noch nie gesichert",
    ok: (m: Moment, dayMonth: string) =>
      m.rel === "today" ? `Heute ${m.time} gesichert` : m.rel === "yesterday" ? `Gestern ${m.time} gesichert` : `${dayMonth} ${m.time} gesichert`,
    serverReachable: (time: string) => `Erreichbar, geprüft um ${time}`,
    serverReachableDay: (day: string) => `Erreichbar, geprüft am ${day}`,
    serverUnreachable: (time: string) => `Nicht erreichbar, geprüft um ${time}`,
    serverUnreachableDay: (day: string) => `Nicht erreichbar, geprüft am ${day}`,
    serverUnknown: "Noch nicht geprüft",
  },
  self: {
    kicker: "Dieser PC",
    runNow: "Jetzt sichern",
    starting: "Wird gestartet …",
    restore: "Dateien wiederherstellen",
    runConflict: "Es läuft bereits eine Sicherung.",
    runStartFailed: (msg: string) => `Die Sicherung ließ sich nicht starten. ${msg}`,
    unconfiguredLead: "nestegg ist auf diesem PC noch nicht eingerichtet.",
    unconfiguredText:
      "Eingerichtet wird im Terminal. Welche Angaben dafür nötig sind (Server, Datastore, Zugang), zeigt dieser Befehl. Danach steht hier, wann zuletzt gesichert wurde.",
    keyLead: "Sicherungen warten auf die Bestätigung der Schlüsselkopie.",
    keyText:
      "nestegg sichert diesen PC erst, wenn du bestätigt hast, dass eine Kopie des Schlüssels außerhalb des PCs liegt. Das dauert ein paar Minuten im Terminal, siehe unten.",
    lastSuccessSentence: (m: Moment) => `Zuletzt gesichert ${at(m)}.`,
    noSuccessYet: "Bisher gibt es keine erfolgreiche Sicherung.",
    okLead: (m: Moment) => `${cap(at(m))} gesichert.`,
    okText: "Alles in Ordnung.",
    nextRun: (m: Moment) =>
      `Die nächste Sicherung ist ${at(m)} fällig. Ist der PC dann aus, wird sie beim nächsten Einschalten nachgeholt.`,
    nextRunDue: "Die nächste Sicherung ist fällig und startet in Kürze.",
    scheduleOff:
      "Die automatische Sicherung ist ausgeschaltet. Gesichert wird nur, wenn du sie startest.",
    lastAttemptPbs: (m: Moment) => `Der letzte Versuch ${at(m)} hat den Backup-Server nicht erreicht.`,
    autoRetry: "nestegg versucht es stündlich von selbst wieder.",
    failingLead: (m: Moment) => `Die Sicherung ${at(m)} ist fehlgeschlagen.`,
    failingLeadUndated: "Die letzte Sicherung ist fehlgeschlagen.",
    overdueLead: (days: number, hours: number) =>
      days >= 2 ? `Seit ${days} Tagen nicht gesichert.` : `Seit ${hours} Stunden nicht gesichert.`,
    overduePbs:
      "Bei den letzten Versuchen war der Backup-Server nicht erreichbar. Nicht dieser PC hat einen Fehler gemacht.",
    overdueCatchUp: "Solange der PC eingeschaltet ist, holt nestegg die Sicherung von selbst nach.",
    neverLead: "Noch keine Sicherung.",
    neverNext: (m: Moment) => `Die erste Sicherung ist ${at(m)} fällig. Du kannst sie auch jetzt starten.`,
    neverNow: "Du kannst die erste Sicherung jetzt starten.",
    neverAttempt: (m: Moment) => `Der letzte Versuch ${at(m)} hat nicht geklappt.`,
    runningLead: "Sicherung läuft.",
    runningText: "Du kannst diese Seite schließen, die Sicherung läuft im Hintergrund weiter.",
    runningSince: (dur: string, manual: boolean) =>
      `${manual ? "Von Hand gestartet" : "Vom Zeitplan gestartet"}, läuft seit ${dur}`,
    stepWake: "Server wecken",
    stepReach: "Server erreichen",
    stepWakeNow: "Server wird geweckt. Warte auf Antwort.",
    stepReachNow: "Verbindung zum Server wird aufgebaut.",
    stepStartingNow: "Sicherung wird gestartet.",
    stepWakeDone: "Server ist erreichbar",
    stepInventory: "Liste der Programme und Laufwerke erstellen",
    stepInventorySub: "Paketlisten, Dienste, Laufwerke",
    stepUpload: "Geänderte Dateien hochladen",
    stepUploadSub: (size: string) => `${size} hochgeladen`,
    stepUploadCheck: "Dateien werden verglichen",
    stepFinish: "Abschließen",
    stepFinishSub: "Bericht an den Server senden",
    factsTitle: "Über diesen PC",
    factName: "Name",
    factBackupId: "Name auf dem Server",
    factOs: "System",
    factSources: "Gesichert werden",
    factSourcesExtra: "und eine Liste der installierten Programme, Dienste und Laufwerke",
    factOnServer: "Auf dem Server",
    onServer: (size: string, count: number) =>
      `${size} im letzten Stand, ${count} ${plural(count, "Stand", "Stände")} insgesamt`,
    onServerCount: (count: number) => `${count} ${plural(count, "Stand", "Stände")}`,
    factEncryption: "Verschlüsselung",
    encryptionOnKey: "Eingeschaltet. Eine Kopie des Schlüssels ist bestätigt.",
    encryptionOnNoKey: "Eingeschaltet. Eine Kopie des Schlüssels ist noch nicht bestätigt.",
    encryptionOff: "Ausgeschaltet. Die Sicherungen liegen unverschlüsselt auf dem Server.",
  },
  key: {
    title: "Eine Kopie des Schlüssels ist noch nicht bestätigt",
    text:
      "Die Sicherungen dieses PCs werden verschlüsselt. Bisher ist nicht bestätigt, dass es eine Kopie des Schlüssels außerhalb dieses PCs gibt. Falls es keine gibt, ließen sich die Sicherungen nach einem Defekt des PCs nicht mehr öffnen. Deshalb sichert nestegg erst, wenn du eine Kopie bestätigt hast.",
    steps: "Im Terminal, nacheinander:",
    paperkey: "Schlüssel als Papierkopie ausgeben, ausdrucken und sicher aufbewahren.",
    confirm: "Danach bestätigen, dass die Kopie sicher liegt.",
    otherTitle: "Keine Kopie des Schlüssels bestätigt",
    otherText: (name: string) =>
      `Die Sicherungen dieses PCs sind verschlüsselt. Für ${name} ist keine Kopie des Schlüssels außerhalb des PCs bestätigt. Falls es keine gibt, ließen sich seine Sicherungen nach einem Defekt von ${name} nicht mehr öffnen.`,
    otherFix: (name: string) => `Das lässt sich nur an ${name} selbst beheben, mit diesen Befehlen im Terminal:`,
  },
  strip: {
    title: "Letzte 14 Tage",
    ok: "Gesichert",
    fail: "Fehlgeschlagen",
    pbs: "Server nicht erreichbar",
    none: "Keine Sicherung",
    run: "Läuft gerade",
  },
  runs: {
    succeeded: "Gesichert",
    succeededManual: "Gesichert, von Hand gestartet",
    succeededWarnings: (n: number) => `Gesichert, mit ${n} ${plural(n, "Hinweis", "Hinweisen")}`,
    failed: (reason: string) => `Fehlgeschlagen: ${reason}`,
    pbs: "Server nicht erreichbar",
    pbsTimes: (n: number) => `Server nicht erreichbar, ${n} Versuche`,
    cancelled: "Abgebrochen",
    uploaded: (size: string) => `${size} hochgeladen`,
    showLog: "Protokoll zeigen",
    hideLog: "Protokoll ausblenden",
    logLoading: "Protokoll wird geladen …",
    logEmpty: "Zu diesem Lauf gibt es kein Protokoll.",
    logFailed: "Das Protokoll ließ sich nicht laden.",
    warnings: "Hinweise",
    showAll: (n: number) => `Alle ${n} Einträge zeigen`,
    showFewer: "Nur die letzten 7 zeigen",
    empty: "Noch keine Sicherungen.",
  },
  reason: {
    pbs_unreachable: "Server nicht erreichbar",
    authentication: "Zugang abgelehnt",
    source_missing: "Ordner fehlte",
    client_missing: "Backup-Programm fehlt",
    client_failed: "Backup-Programm meldete einen Fehler",
    already_running: "lief schon",
    config: "Einrichtung unvollständig",
    internal: "interner Fehler",
  } satisfies Record<RunErrorKind, string>,
  explain: {
    pbs_unreachable:
      "Der Backup-Server hat nicht geantwortet. Nicht dieser PC hat einen Fehler gemacht.",
    authentication:
      "Der Backup-Server hat den Zugang abgelehnt. Wahrscheinlich wurde das API-Token geändert oder gelöscht. Neu hinterlegen lässt es sich mit sudo nestegg credentials.",
    source_missing:
      "Ein Ordner, der gesichert werden soll, fehlte, zum Beispiel weil eine Platte nicht eingehängt war. nestegg bricht dann ab, statt eine unvollständige Sicherung als erfolgreich zu melden.",
    client_missing:
      "Das Programm proxmox-backup-client ist nicht installiert. nestegg braucht es für jede Sicherung.",
    client_failed:
      "Das Backup-Programm hat mit einem Fehler aufgehört. Was genau passiert ist, steht im Protokoll.",
    already_running: "Zu dieser Zeit lief schon eine andere Sicherung. Es ist nichts verloren gegangen.",
    config:
      "Die Einrichtung ist unvollständig. Was fehlt, steht im Protokoll; sudo nestegg check zeigt es ebenfalls.",
    internal: "In nestegg selbst ist ein Fehler aufgetreten. Was genau passiert ist, steht im Protokoll.",
  } satisfies Record<RunErrorKind, string>,
  other: {
    kicker: "Nur zur Ansicht, wird am PC selbst verwaltet",
    failingLead: (m: Moment) => `Die Sicherung ${at(m)} ist fehlgeschlagen.`,
    overdueLead: (days: number, hours: number) =>
      days >= 2 ? `Seit ${days} Tagen nicht gesichert.` : `Seit ${hours} Stunden nicht gesichert.`,
    overdueText: (m: Moment) => `Zuletzt gesichert ${at(m)}. Seitdem ist keine neue Sicherung auf dem Server angekommen.`,
    neverLead: "Auf dem Server gibt es noch keine Sicherung dieses PCs.",
    neverText: "Sobald er seine erste Sicherung gemacht hat, steht hier mehr.",
    okLead: (m: Moment) => `${cap(at(m))} gesichert.`,
    okText: "Alles in Ordnung, soweit der Backup-Server es sehen kann.",
    pbsText: "Beim letzten Versuch war der Backup-Server nicht erreichbar.",
    lastSuccess: (m: Moment) => `Zuletzt erfolgreich gesichert ${at(m)}.`,
    reportedError: "Gemeldeter Fehler",
    factsTitle: "Was der Server über diesen PC weiß",
    factKey: "Kopie des Schlüssels bestätigt",
    yes: "Ja",
    no: "Nein",
    unknown: "Unbekannt",
    factRead: "Stand",
    readAt: (m: Moment) => `Vom Server gelesen ${at(m)}`,
    factVersion: "nestegg-Version",
    notFound: "Diesen PC gibt es in der Liste nicht (mehr).",
  },
  server: {
    title: "Backup-Server",
    reachableLead: "Bei der letzten Prüfung erreichbar.",
    unreachableLead: "Bei der letzten Prüfung nicht erreichbar.",
    unknownLead: "Noch nicht geprüft.",
    checked: (m: Moment) => `Geprüft ${at(m)}.`,
    unknownText: "nestegg prüft die Verbindung bei jeder Sicherung.",
    contactsTitle: "Bei den letzten Sicherungen",
    contactsText: "So hat der Server bei den letzten Sicherungen dieses PCs reagiert.",
    contactOk: "Erreicht",
    contactPbs: "Nicht erreicht, keine Sicherung",
    contactPbsTimes: (n: number) => `Nicht erreicht, ${n} Versuche`,
    contactUnknown: "Nicht geprüft, der Lauf endete vorher",
    contactsEmpty: "Noch keine Sicherung versucht.",
    storageTitle: "Ablage",
    address: "Adresse",
    datastore: "Datastore",
    namespace: "Namespace",
    noNamespace: "keiner",
    wol: "Wake-on-LAN",
    on: "Eingeschaltet",
    off: "Ausgeschaltet",
    retention: "Aufbewahrung",
    retentionText: "Wird auf dem Server eingestellt.",
  },
  settings: {
    title: "Einstellungen",
    intro:
      "So ist nestegg auf diesem PC eingerichtet. Ändern lässt sich das im Terminal mit sudo nestegg init oder in der Datei /etc/nestegg/config.toml.",
    checkHint: "Ob Zugang und Server funktionieren, prüft dieser Befehl:",
    server: "Backup-Server",
    backupId: "Name auf dem Server",
    wake: "Server aufwecken",
    when: "Wann gesichert wird",
    interval: "Rhythmus",
    everyHours: (h: number) => (h === 24 ? "Einmal am Tag" : h % 24 === 0 ? `Alle ${h / 24} Tage` : `Alle ${h} Stunden`),
    automatic: "Automatisch",
    automaticOn:
      "Eingeschaltet. nestegg prüft stündlich, ob eine Sicherung fällig ist, und holt verpasste nach.",
    automaticOff: "Ausgeschaltet. Der Zeitplan (nestegg-backup.timer) ist nicht aktiv.",
    nextRun: "Nächste Sicherung",
    overdueAfter: "Überfällig nach",
    overdueHours: (h: number) => `${h} Stunden ohne erfolgreiche Sicherung`,
    what: "Was gesichert wird",
    folders: "Ordner",
    extra: "Zusätzlich",
    extraText:
      "Liste der installierten Programme, Dienste und Laufwerke, damit sich der PC nach einer Neuinstallation wieder einrichten lässt",
    excluded: "Ausgenommen",
    nothing: "nichts",
    encryption: "Verschlüsselung",
    fingerprint: "Fingerabdruck des Schlüssels",
    version: (v: string, api: string) => `nestegg ${v}, API ${api}`,
    unconfigured: "Noch nicht eingerichtet. Welche Angaben nötig sind, zeigt dieser Befehl:",
  },
  restore: {
    title: "Dateien wiederherstellen",
    cancel: "Abbrechen",
    step: (n: number, title: string) => `Schritt ${n} von 3: ${title}`,
    stepTitles: ["Stand wählen", "Dateien wählen", "Zielordner festlegen"],
    pickIntro:
      "Von welchem Tag sollen die Dateien stammen? Nimm den letzten Stand, an dem sie noch in Ordnung waren.",
    newest: "Neueste Stände",
    older: (n: number, until: string) => `Ältere Stände zeigen (${n} weitere, bis ${until})`,
    loadingSnapshots: "Stände werden vom Server geholt …",
    noSnapshots: "Auf dem Server gibt es noch keinen Stand dieses PCs.",
    pbsUnreachable: "Der Backup-Server ist gerade nicht erreichbar. Wiederherstellen geht nur, wenn er läuft.",
    pbsUnreachableWol:
      "Wake-on-LAN ist eingerichtet: Eine Sicherung versucht zuerst, ihn zu wecken. Danach kannst du es hier erneut versuchen.",
    showFiles: "Dateien dieses Stands anzeigen",
    notAvailable:
      "Diese nestegg-Version kann Dateien noch nicht über die Oberfläche wiederherstellen. Im Terminal geht es schon, zum Beispiel für den neuesten Stand:",
    notAvailableTarget: "<NEUER-ORDNER>",
    snapshotOf: (day: string, time: string) => `Stand vom ${day}, ${time}.`,
    otherSnapshot: "Anderen Stand wählen",
    inventory: "Liste der Programme, Dienste und Laufwerke",
    expand: (name: string) => `${name} aufklappen`,
    collapse: (name: string) => `${name} zuklappen`,
    treeLabel: "Dateien im Stand",
    dirLoading: "Wird geladen …",
    dirEmpty: "Leerer Ordner",
    dirFailed: "Dieser Ordner ließ sich nicht öffnen.",
    oneArea: (area: string) =>
      `Eine Wiederherstellung holt Dateien aus einem Bereich. Für ${area} danach eine zweite starten.`,
    nothingSelected: "Noch nichts ausgewählt",
    selected: (n: number, size: string | null) =>
      `${n} ${plural(n, "Element", "Elemente")} ausgewählt${size ? `, zusammen ${size}` : ""}`,
    toTarget: "Zielordner festlegen",
    targetIntro:
      "nestegg legt einen neuen Ordner an und schreibt nur dort hinein. Vorhandene Dateien werden nie überschrieben. Danach kannst du dir in Ruhe heraussuchen, was du brauchst.",
    folderLabel: "Name des neuen Ordners",
    pathLabel: "Neuer Ordner (vollständiger Pfad)",
    folderEmpty: "Bitte einen Namen für den neuen Ordner angeben.",
    folderSlash: "Der Name darf keinen Schrägstrich enthalten.",
    folderInvalid: "Dieser Name geht nicht, bitte einen anderen wählen.",
    pathNotAbsolute: "Bitte einen vollständigen Pfad angeben, der mit / beginnt.",
    folderOk: (user: string) => `Diesen Ordner darf es noch nicht geben. Er gehört nachher dir (${user}).`,
    pathOk: "Diesen Ordner darf es noch nicht geben; nestegg legt ihn neu an.",
    targetExists: "Diesen Ordner gibt es schon. nestegg schreibt nur in einen neuen Ordner, bitte einen anderen Namen wählen.",
    summary: "Zusammenfassung",
    sumSnapshot: "Stand",
    sumSelection: "Auswahl",
    sumSize: "Größe",
    sumTarget: "Zielordner",
    changeSelection: "Auswahl ändern",
    start: "In den neuen Ordner wiederherstellen",
    starting: "Wird gestartet …",
    running: "Dateien werden zurückgeholt.",
    runningTarget: "Ziel",
    runningSince: (dur: string) => `Läuft seit ${dur}`,
    done: "Fertig. Die Dateien liegen jetzt in",
    doneText: (n: number, day: string) =>
      `${n} ${plural(n, "Element", "Elemente")}, Stand vom ${day}. An deinen übrigen Dateien wurde nichts verändert.`,
    failed: "Das Wiederherstellen ist fehlgeschlagen.",
    backTo: (name: string) => `Zurück zu ${name}`,
    tryAgain: "Zurück zum Zielordner",
    startFailed: (msg: string) => `Das Wiederherstellen ließ sich nicht starten. ${msg}`,
  },
};

export default de;
export type Messages = typeof de;
