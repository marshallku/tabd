import test from "node:test";
import assert from "node:assert/strict";
import { compileUrlMatcher, globToRegExp } from "../../shared/urlMatch.js";

test("exact matcher requires identical URL", () => {
  const match = compileUrlMatcher("https://example.com/", "exact");
  assert.equal(match("https://example.com/"), true);
  assert.equal(match("https://example.com"), false);
  assert.equal(match("https://example.com/x"), false);
});

test("glob matcher treats * as wildcard", () => {
  const match = compileUrlMatcher("https://example.com/dash*", "glob");
  assert.equal(match("https://example.com/dash"), true);
  assert.equal(match("https://example.com/dashboard"), true);
  assert.equal(match("https://example.com/dashboard?x=1"), true);
  assert.equal(match("https://example.com/other"), false);
});

test("glob matcher escapes regex meta-characters", () => {
  const match = compileUrlMatcher("https://example.com/foo.bar+baz", "glob");
  assert.equal(match("https://example.com/foo.bar+baz"), true);
  // dot/plus must not act as regex metachars
  assert.equal(match("https://example.com/fooXbarbaz"), false);
  assert.equal(match("https://example.com/foo.barbaz"), false);
});

test("glob matcher treats ? as single-char wildcard", () => {
  const match = compileUrlMatcher("https://example.com/a?c", "glob");
  assert.equal(match("https://example.com/abc"), true);
  assert.equal(match("https://example.com/aXc"), true);
  assert.equal(match("https://example.com/ac"), false);
  assert.equal(match("https://example.com/abbc"), false);
});

test("regex matcher uses test semantics (substring allowed)", () => {
  const match = compileUrlMatcher("\\/dashboard($|\\?)", "regex");
  assert.equal(match("https://example.com/dashboard"), true);
  assert.equal(match("https://example.com/dashboard?ref=x"), true);
  assert.equal(match("https://example.com/dashboards"), false);
});

test("regex matcher surfaces a clear error on invalid pattern", () => {
  assert.throws(() => compileUrlMatcher("(unclosed", "regex"), /Invalid regex/);
});

test("empty pattern is rejected", () => {
  assert.throws(() => compileUrlMatcher("", "exact"), /pattern is required/);
});

test("globToRegExp anchors at both ends", () => {
  const re = globToRegExp("foo*");
  assert.equal(re.test("foo"), true);
  assert.equal(re.test("foobar"), true);
  assert.equal(re.test("xfoo"), false);
});
