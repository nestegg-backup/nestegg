// Stand-in for the daemon's rule about excluding a whole folder (decision 0022).
//
// The daemon runs the backup client's own matcher against the real accounts and the configured
// sources; the mock cannot. It refuses a small fixed set of patterns that the daemon refuses on a
// default installation, with the daemon's message, and accepts everything else:
//
//   - for the home directory /home/alice (source /home, archive home.pxar):
//       /alice   alice   **/alice   /[a-z]*
//   - for each backed-up folder S (for example /home or /usr/local):
//       S   S/   S/*   S/**
//
// A pattern that is already in the list is not judged again (the daemon keeps what root put in the
// file); the caller handles that.

const HOME = "/home/alice";
const HOME_PATTERNS = ["/alice", "alice", "**/alice", "/[a-z]*"];

/** The daemon's refusal for a newly added pattern, or null (the mock accepts the rest). */
export function wholeFolderProblem(pattern: string, sources: readonly string[]): string | null {
  const folderOf = (): string | null => {
    if (HOME_PATTERNS.includes(pattern)) return HOME;
    for (const source of sources) {
      const path = source.length > 1 ? source.replace(/\/+$/, "") : source;
      if ([path, `${path}/`, `${path}/*`, `${path}/**`].includes(pattern)) return source;
    }
    return null;
  };
  const folder = folderOf();
  if (folder === null) return null;
  return `exclude pattern \`${pattern}\` would leave out the whole folder ${folder} from the backup; an administrator can still set it as root in /etc/nestegg/config.toml.`;
}
