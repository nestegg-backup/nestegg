// Stand-in for the daemon's rule about excluding a whole folder (decision 0022).
//
// The daemon runs the backup client's own matcher against the real accounts and the configured
// sources; the mock cannot. It refuses a small fixed set of patterns that the daemon refuses on a
// default installation, with the daemon's message, and accepts everything else:
//
//   - for the home directory /home/alice (source /home, archive home.pxar):
//       /alice   alice   **/alice   /[a-z]*
//
// Patterns are relative to the root of each archive, so `/home` or `/etc/**` match nothing in the
// archives of this setup and are accepted, as by the daemon. A pattern the backup client cannot
// parse (unclosed character class, trailing backslash) is refused too (`malformedProblem`).
//
// A pattern that is already in the list is not judged again (the daemon keeps what root put in the
// file); the caller handles that.

const HOME = "/home/alice";
const HOME_PATTERNS = ["/alice", "alice", "**/alice", "/[a-z]*"];

/** The daemon's refusal for a newly added pattern, or null (the mock accepts the rest). */
export function wholeFolderProblem(pattern: string, sources: readonly string[]): string | null {
  // Patterns are relative to each archive's root: in `home.pxar` (made from
  // /home) the entry is `/alice`. A pattern such as `/home` or `/etc/**`
  // matches nothing in the archives of this (non-system) setup, so the daemon
  // accepts it; only the home directory can be lost here.
  void sources;
  const folder = HOME_PATTERNS.includes(pattern) ? HOME : null;
  if (folder === null) return null;
  // The daemon judges the whole new list and words it accordingly.
  return `this exclude list would leave out the whole folder ${folder} from the backup; an administrator can still set it as root in /etc/nestegg/config.toml.`;
}

/**
 * The daemon's refusal for a pattern the backup client cannot parse, or null.
 * Stand-in: an unclosed character class or a trailing backslash.
 */
export function malformedProblem(pattern: string): string | null {
  const open = (pattern.match(/\[/g) ?? []).length;
  const close = (pattern.match(/\]/g) ?? []).length;
  if (open > close || /\[[^\]]*-\]/.test(pattern))
    return `exclude pattern \`${pattern}\` is malformed (an unclosed \`[\`…\`]\` character class or a dangling \`\\\`, for example) and would break every backup run`;
  if (pattern.endsWith("\\"))
    return `exclude pattern \`${pattern}\` is not a valid pattern (trailing backslash); the backup client would reject it`;
  return null;
}
