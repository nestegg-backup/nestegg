// German UI strings. The wording follows the design prototypes (decisions 0005 and 0014).
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
    version: (v: string) => `nestegg ${v}`,
  },
  theme: {
    label: "Darstellung",
    system: "System",
    light: "Hell",
    dark: "Dunkel",
  },
  title: {
    selfSub: (os: string | null) => (os ? `dieser PC, ${os}` : "dieser PC"),
    otherSub: "nur zur Ansicht",
  },
  sbar: {
    serverReachable: (m: Moment, dayMonth: string) =>
      m.rel === "today" ? `Backup-Server erreichbar, geprüft heute um ${m.time}` : `Backup-Server erreichbar, geprüft am ${dayMonth}`,
    serverUnreachable: (m: Moment, dayMonth: string) =>
      m.rel === "today"
        ? `Backup-Server nicht erreichbar, geprüft heute um ${m.time}`
        : `Backup-Server nicht erreichbar, geprüft am ${dayMonth}`,
    serverUnknown: "Backup-Server noch nicht geprüft",
    offline: "Keine Verbindung zum nestegg-Dienst",
    sessionEnded: "Sitzung beendet",
    saved: (time: string, what: string) => `Gespeichert um ${time}: ${what}`,
    notSaved: (what: string) => `Nicht gespeichert: ${what}`,
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
  },
  row: {
    running: "Sichert gerade",
    unconfigured: "Noch nicht eingerichtet",
    keyMissing: "Schlüsselkopie nicht bestätigt",
    failing: (day: string) => `Fehlgeschlagen am ${day}`,
    failingUndated: "Letzte Sicherung fehlgeschlagen",
    overdue: (day: string) => `Überfällig, zuletzt ${day}`,
    never: "Noch nie gesichert",
    neverServer: "Frühere Stände auf dem Server",
    overdueUndated: "Überfällig",
    okUndated: "Gesichert",
    ok: (m: Moment, dayMonth: string) =>
      m.rel === "today" ? `Heute ${m.time} gesichert` : m.rel === "yesterday" ? `Gestern ${m.time} gesichert` : `${dayMonth} ${m.time} gesichert`,
    serverReachable: (time: string) => `Erreichbar, geprüft um ${time}`,
    serverReachableDay: (day: string) => `Erreichbar, geprüft am ${day}`,
    serverUnreachable: (time: string) => `Nicht erreichbar, geprüft um ${time}`,
    serverUnreachableDay: (day: string) => `Nicht erreichbar, geprüft am ${day}`,
    serverUnknown: "Noch nicht geprüft",
  },
  self: {
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
    okLeadUndated: "Gesichert.",
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
    neverLeadServer: "Von dieser Installation gibt es noch keine Sicherung.",
    neverServer: (count: number, m: Moment) =>
      `Auf dem Server ${count === 1 ? "liegt ein früherer Stand" : `liegen ${count} frühere Stände`} dieses PCs. Der neueste entstand ${at(m)}. Daraus kannst du Dateien wiederherstellen.`,
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
    factBackupId: "Name auf dem Server",
    factOs: "System",
    factSources: "Gesichert werden",
    factSourcesExtra: "und eine Liste der installierten Programme, Dienste und Laufwerke",
    factOnServer: "Auf dem Server",
    factNext: "Nächste Sicherung",
    nextAt: (m: Moment) => cap(at(m)),
    nextDue: "Fällig, startet in Kürze",
    nextOff: "Keine, automatisch sichern ist aus",
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
    title: "Verlauf",
    colDay: "Tag",
    colStart: "Start",
    colResult: "Ergebnis",
    colDuration: "Dauer",
    colUploaded: "Hochgeladen",
    showLog: "Protokoll",
    hideLog: "Protokoll ausblenden",
    logLoading: "Protokoll wird geladen …",
    logEmpty: "Zu diesem Lauf gibt es kein Protokoll.",
    logFailed: "Das Protokoll ließ sich nicht laden.",
    warnings: "Hinweise",
    showAll: (n: number) => `Alle ${n} Einträge zeigen`,
    showFewer: "Weniger zeigen",
    empty: "Noch keine Sicherungen.",
    emptyHere: "Noch keine Sicherung von dieser Installation. Die früheren liegen auf dem Server.",
  },
  reason: {
    pbs_unreachable: "Server nicht erreichbar",
    authentication: "Zugang abgelehnt",
    source_missing: "Ordner fehlte",
    client_missing: "Backup-Programm fehlt",
    client_failed: "Backup-Programm meldete einen Fehler",
    already_running: "lief schon",
    config: "Einrichtung unvollständig",
    credentials_unreadable: "Zugangsdaten nicht lesbar",
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
    credentials_unreadable:
      "Dieser PC kann das gespeicherte Token-Secret oder den Schlüssel nicht mehr entschlüsseln, zum Beispiel nach einem Mainboard-Tausch oder einer Wiederherstellung auf anderer Hardware. Neu hinterlegen mit sudo nestegg credentials set-password und sudo nestegg key import mit der exportierten Schlüsseldatei.",
    internal: "In nestegg selbst ist ein Fehler aufgetreten. Was genau passiert ist, steht im Protokoll.",
  } satisfies Record<RunErrorKind, string>,
  other: {
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
    intro: "Änderungen werden sofort gespeichert und gelten ab der nächsten Sicherung.",
    introReadOnly:
      "Diese nestegg-Version zeigt die Einstellungen nur an. Ändern lässt sich das im Terminal mit sudo nestegg init oder in der Datei /etc/nestegg/config.toml.",
    offline: "Ohne Verbindung zum nestegg-Dienst lässt sich gerade nichts ändern.",
    unconfigured: "Noch nicht eingerichtet. Welche Angaben nötig sind, zeigt dieser Befehl:",
    schedule: "Zeitplan",
    automatic: "Automatisch sichern",
    automaticOn: "nestegg prüft stündlich, ob eine Sicherung fällig ist, und holt verpasste nach.",
    automaticOff: "Gesichert wird nur, wenn du es selbst startest.",
    interval: "Rhythmus",
    intervalHint: "Wie oft eine Sicherung fällig ist.",
    everyHours: (h: number) =>
      h === 1
        ? "Stündlich"
        : h === 24
          ? "Täglich"
          : h === 168
            ? "Wöchentlich"
            : h % 24 === 0
              ? `Alle ${h / 24} Tage`
              : `Alle ${h} Stunden`,
    overdue: "Überfällig melden nach",
    overdueHint: "Wird gespeichert, wenn du das Feld verlässt oder Enter drückst.",
    hours: "Stunden",
    excludes: "Ausgeschlossene Ordner",
    excludesHint: (sources: string) => `Was zu diesen Mustern passt, wird nicht gesichert. Gesichert werden ${sources}.`,
    excludesHintPlain: "Was zu diesen Mustern passt, wird nicht gesichert.",
    excludesNone: "Bisher ist nichts ausgeschlossen.",
    remove: "Entfernen",
    removeLabel: (pattern: string) => `${pattern} entfernen`,
    add: "Hinzufügen",
    newPattern: "Neues Muster",
    placeholder: "zum Beispiel **/Downloads",
    wake: "Server aufwecken",
    wol: "Wake-on-LAN",
    wolHint: "Schläft der Backup-Server, schickt nestegg vor jeder Sicherung ein Weck-Signal und wartet, bis er antwortet.",
    wolNotConfigured:
      "Nicht eingerichtet. Dafür braucht nestegg die MAC-Adresse des Backup-Servers. Sie wird im Terminal eingetragen (Abschnitt [wake] in /etc/nestegg/config.toml), danach lässt sich Wake-on-LAN hier ein- und ausschalten.",
    on: "Eingeschaltet",
    off: "Ausgeschaltet",
    fixed: "Server und Verschlüsselung",
    fixedHint:
      "Nur zum Nachsehen. Weil Zugangsdaten und Schlüssel daran hängen, ändert man das im Terminal mit Administratorrechten.",
    changeIn: "Ändern im Terminal:",
    checkIn: "Prüfen, ob Zugang und Server funktionieren:",
    address: "Server-Adresse",
    backupId: "Name auf dem Server",
    encryption: "Verschlüsselung",
    fingerprint: "Fingerabdruck des Schlüssels",
    retention: "Aufbewahrung",
    retentionText: "Wird auf dem Server eingestellt",
    saved: "Gespeichert",
    saving: "Wird gespeichert …",
    notSaved: (reason: string) => `Nicht gespeichert. ${reason}`,
    sumAutomatic: (on: boolean) => `Automatisch sichern ${on ? "eingeschaltet" : "ausgeschaltet"}`,
    sumInterval: (label: string) => `Rhythmus ${label.charAt(0).toLowerCase()}${label.slice(1)}`,
    sumIntervalOverdue: (label: string, hours: number) =>
      `Rhythmus ${label.charAt(0).toLowerCase()}${label.slice(1)}, überfällig melden nach ${hours} Stunden`,
    sumOverdue: (hours: number) => `Überfällig melden nach ${hours} Stunden`,
    sumExcludeAdded: (pattern: string) => `${pattern} wird ab jetzt ausgelassen`,
    sumExcludeRemoved: (pattern: string) => `${pattern} wird wieder gesichert`,
    sumWol: (on: boolean) => `Wake-on-LAN ${on ? "eingeschaltet" : "ausgeschaltet"}`,
    overdueRaised: (hours: number) =>
      `Auf ${hours} Stunden angehoben, weil es länger sein muss als der Rhythmus.`,
    problem: {
      wholeNumber: "Bitte eine ganze Zahl von Stunden eingeben.",
      intervalRange: (min: number, max: number) => `Der Rhythmus muss zwischen ${min} und ${max} Stunden liegen.`,
      overdueRange: (min: number, max: number) => `Erlaubt sind ${min} bis ${max} Stunden.`,
      overdueNotGreater: (interval: number) =>
        `Muss länger sein als der Rhythmus (${interval} Stunden). Sonst gilt jede Sicherung schon als überfällig, bevor die nächste fällig ist.`,
      excludeEmpty: "Bitte ein Muster eingeben, zum Beispiel **/.cache.",
      excludeTooLong: (n: number) => `Ein Muster darf höchstens ${n} Zeichen lang sein.`,
      excludeControl: "Das Muster enthält einen Zeilenumbruch oder ein anderes Steuerzeichen.",
      excludeSpaces: "Das Muster darf nicht mit einem Leerzeichen beginnen oder enden.",
      excludeDash: "Das Muster darf nicht mit einem Minuszeichen beginnen.",
      excludeEverything: "Damit würde nichts mehr gesichert. Bitte ein engeres Muster wählen.",
      excludeDuplicate: "Dieses Muster ist schon in der Liste.",
      excludeTooMany: (n: number) => `Mehr als ${n} Muster sind nicht möglich.`,
      wakeNotConfigured:
        "Wake-on-LAN ist nicht eingerichtet: Die MAC-Adresse des Backup-Servers fehlt. Sie wird im Terminal eingetragen.",
      terminalOnly: "Das lässt sich nur im Terminal ändern.",
      timerHalf:
        "Gespeichert, aber der Zeitplan ließ sich nicht starten oder anhalten. Prüfe es im Terminal mit systemctl status nestegg-backup.timer.",
      daemon: "Der nestegg-Dienst antwortet nicht. Versuche es gleich noch einmal.",
      session: "Die Sitzung ist beendet. Öffne die Adresse, die nestegg ui im Terminal ausgegeben hat.",
      forbidden:
        "nestegg konnte nicht feststellen, wer die Änderung schickt. Starte nestegg ui neu und versuche es noch einmal.",
      timer: "Die automatische Sicherung ließ sich nicht umschalten. Es wurde nichts geändert.",
      configBroken:
        "Die Einrichtungsdatei ist fehlerhaft. Bitte zuerst im Terminal reparieren; sudo nestegg check zeigt, was nicht stimmt.",
      other: "Der nestegg-Dienst hat die Änderung abgelehnt.",
    },
  },
  restore: {
    title: "Dateien wiederherstellen",
    cancel: "Abbrechen",
    stepOf: (n: number) => `Schritt ${n} von 3`,
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
