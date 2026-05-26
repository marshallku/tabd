export type UrlPatternType = "exact" | "glob" | "regex";

const GLOB_SPECIAL = /[.+^${}()|[\]\\]/g;

export function compileUrlMatcher(
  pattern: string,
  patternType: UrlPatternType = "exact"
): (url: string) => boolean {
  if (!pattern) {
    throw new Error("pattern is required");
  }
  switch (patternType) {
    case "exact":
      return (url) => url === pattern;
    case "regex": {
      let re: RegExp;
      try {
        re = new RegExp(pattern);
      } catch (err) {
        throw new Error(
          `Invalid regex pattern: ${err instanceof Error ? err.message : String(err)}`
        );
      }
      return (url) => re.test(url);
    }
    case "glob": {
      const re = globToRegExp(pattern);
      return (url) => re.test(url);
    }
    default:
      throw new Error(`Unsupported patternType: ${String(patternType)}`);
  }
}

export function globToRegExp(pattern: string): RegExp {
  // Translate ? and * to regex equivalents while escaping every other special
  // character. ** behaves the same as * since URL globs are flat strings.
  let body = "";
  for (const ch of pattern) {
    if (ch === "*") {
      body += ".*";
    } else if (ch === "?") {
      body += ".";
    } else {
      body += ch.replace(GLOB_SPECIAL, "\\$&");
    }
  }
  return new RegExp(`^${body}$`);
}
