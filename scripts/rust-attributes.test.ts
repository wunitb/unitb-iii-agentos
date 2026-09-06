import { describe, expect, test } from "vitest";
import { countRustAttributes } from "./rust-attributes.js";

describe("Rust source attribute counter", () => {
  test("ignores attribute-shaped text in strings, raw strings, and comments", () => {
    const manyHashes = "#".repeat(17);
    const source = String.raw`
      const QUOTED: &str = "#[test] and #[ignore]";
      const ESCAPED: &str = "escaped quote: \" #[tokio::test(flavor = \"current_thread\")]";
      const MULTILINE: &str = "first line
        #[test]
        #[ignore]
      last line";
      const BYTES: &[u8] = b"#[tokio::test]";
      const C_STRING: &CStr = c"#[test]";
      const RAW_ZERO: &str = r"#[test]";
      const RAW: &str = r###"#[tokio::test(flavor = "multi_thread")] #[ignore]"###;
      const BYTE_RAW: &[u8] = br##"#[test]"##;
      const C_RAW: &CStr = cr####"#[ignore]"####;
      const MANY_HASHES: &str = r${manyHashes}"#[test]"${manyHashes};
      // #[test] #[tokio::test] #[ignore]
      /* outer #[test]
         /* nested #[tokio::test] */
         still outer #[ignore]
      */

      #[test]
      fn real_after_fixtures() {}
      #[tokio::test(flavor = "current_thread")]
      async fn real_async_after_fixtures() {}
      #[ignore]
      #[test]
      fn real_ignored_after_fixtures() {}
    `;

    expect(countRustAttributes(source)).toEqual({ tests: 3, ignored: 1 });
  });

  test("keeps real direct attributes after chars, byte chars, and lifetimes", () => {
    const source = String.raw`
      fn borrow<'a>(value: &'a str) -> &'a str { value }
      let quote = '\'';
      let hash = '#';
      let byte = b'#';

      #[test]
      fn direct() {}

      #[tokio::test(flavor = "current_thread")]
      async fn asynchronous() {}

      #[ignore = "bounded reason"]
      #[test]
      fn ignored_direct() {}
    `;

    expect(countRustAttributes(source)).toEqual({ tests: 3, ignored: 1 });
  });

  test("does not invent attributes after unclosed lexical constructs", () => {
    for (const source of [
      'const S: &str = "unclosed #[test] #[ignore]',
      'const S: &str = r###"unclosed #[tokio::test] #[ignore]',
      '/* unclosed /* nested */ #[test] #[ignore]',
      "let bad = b'unclosed #[test] #[ignore]",
      "let fake = '#[test]'",
      "let fake = b'#[ignore]'",
    ]) {
      expect(countRustAttributes(source)).toEqual({ tests: 0, ignored: 0 });
    }
  });

  test("does not let lifetime tokens swallow later attributes", () => {
    const source = String.raw`
      fn first<'a, 'b>(left: &'a str, right: &'b str) -> (&'a str, &'b str) {
        (left, right)
      }
      #[test]
      fn after_lifetimes() {}
      #[tokio::test]
      async fn after_more_lifetimes() { let _: &'static str = "ok"; }
    `;

    expect(countRustAttributes(source)).toEqual({ tests: 2, ignored: 0 });
  });
});
