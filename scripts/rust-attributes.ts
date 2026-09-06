export interface RustAttributeCounts {
  readonly tests: number;
  readonly ignored: number;
}

function isIdentifierStart(character: string | undefined): boolean {
  return character !== undefined && /[A-Za-z_]/.test(character);
}

function isIdentifierContinue(character: string | undefined): boolean {
  return character !== undefined && /[A-Za-z0-9_]/.test(character);
}

function maskRange(characters: string[], source: string, start: number, end: number): void {
  for (let index = start; index < end; index += 1) {
    if (source[index] !== "\n" && source[index] !== "\r") characters[index] = " ";
  }
}

function rawStringStart(source: string, index: number): { quote: number; hashes: number } | null {
  if (index > 0 && isIdentifierContinue(source[index - 1])) return null;

  let cursor: number;
  if (source[index] === "r") cursor = index + 1;
  else if ((source[index] === "b" || source[index] === "c") && source[index + 1] === "r") {
    cursor = index + 2;
  } else return null;

  const hashStart = cursor;
  while (source[cursor] === "#") cursor += 1;
  if (source[cursor] !== '"') return null;
  return { quote: cursor, hashes: cursor - hashStart };
}

function rawStringEnd(source: string, quote: number, hashes: number): number {
  const suffix = '"' + "#".repeat(hashes);
  const end = source.indexOf(suffix, quote + 1);
  return end < 0 ? source.length : end + suffix.length;
}

function quotedStringEnd(source: string, quote: number): number {
  let cursor = quote + 1;
  while (cursor < source.length) {
    if (source[cursor] === "\\") {
      cursor += Math.min(2, source.length - cursor);
    } else if (source[cursor] === '"') {
      return cursor + 1;
    } else {
      cursor += 1;
    }
  }
  return source.length;
}

function escapedCharacterEnd(source: string, slash: number): number | null {
  const kind = source[slash + 1];
  if (kind === undefined || kind === "\n" || kind === "\r") return null;
  if (kind === "x") {
    return /^[0-9A-Fa-f]{2}$/.test(source.slice(slash + 2, slash + 4)) ? slash + 4 : null;
  }
  if (kind === "u" && source[slash + 2] === "{") {
    const close = source.indexOf("}", slash + 3);
    if (close < 0 || !/^[0-9A-Fa-f_]+$/.test(source.slice(slash + 3, close))) return null;
    return close + 1;
  }
  return slash + 2;
}

function characterLiteralEnd(source: string, quote: number): number | null {
  let cursor = quote + 1;
  if (source[cursor] === "\\") {
    const escapedEnd = escapedCharacterEnd(source, cursor);
    if (escapedEnd === null) return null;
    cursor = escapedEnd;
  } else {
    const codePoint = source.codePointAt(cursor);
    if (codePoint === undefined || source[cursor] === "'" || source[cursor] === "\n" || source[cursor] === "\r") {
      return null;
    }
    cursor += codePoint > 0xffff ? 2 : 1;
  }
  return source[cursor] === "'" ? cursor + 1 : null;
}

/**
 * Replace comments and literals with whitespace while retaining code byte positions.
 * An unclosed literal/comment masks the rest of the input: malformed fixtures must
 * not create measurements from text whose lexical role cannot be established.
 */
export function rustCodeOnly(source: string): string {
  const characters = source.split("");
  let index = 0;

  while (index < source.length) {
    if (source.startsWith("//", index)) {
      const newline = source.indexOf("\n", index + 2);
      const end = newline < 0 ? source.length : newline;
      maskRange(characters, source, index, end);
      index = end;
      continue;
    }

    if (source.startsWith("/*", index)) {
      let cursor = index + 2;
      let depth = 1;
      while (cursor < source.length && depth > 0) {
        if (source.startsWith("/*", cursor)) {
          depth += 1;
          cursor += 2;
        } else if (source.startsWith("*/", cursor)) {
          depth -= 1;
          cursor += 2;
        } else {
          cursor += 1;
        }
      }
      const end = depth === 0 ? cursor : source.length;
      maskRange(characters, source, index, end);
      index = end;
      continue;
    }

    const raw = rawStringStart(source, index);
    if (raw !== null) {
      const end = rawStringEnd(source, raw.quote, raw.hashes);
      maskRange(characters, source, index, end);
      index = end;
      continue;
    }

    let stringQuote: number | null = null;
    if (source[index] === '"') stringQuote = index;
    else if (
      (source[index] === "b" || source[index] === "c") &&
      source[index + 1] === '"' &&
      (index === 0 || !isIdentifierContinue(source[index - 1]))
    ) stringQuote = index + 1;
    if (stringQuote !== null) {
      const end = quotedStringEnd(source, stringQuote);
      maskRange(characters, source, index, end);
      index = end;
      continue;
    }

    let charQuote: number | null = null;
    const byteCharacter = source[index] === "b" && source[index + 1] === "'" &&
      (index === 0 || !isIdentifierContinue(source[index - 1]));
    if (byteCharacter) charQuote = index + 1;
    else if (source[index] === "'") charQuote = index;

    if (charQuote !== null) {
      const end = characterLiteralEnd(source, charQuote);
      if (end !== null) {
        maskRange(characters, source, index, end);
        index = end;
        continue;
      }
      // A leading apostrophe plus identifier is a lifetime or loop label, not
      // an unclosed character literal. Leave it as code so later attributes survive.
      if (!byteCharacter && isIdentifierStart(source[charQuote + 1])) {
        index += 1;
        continue;
      }
      maskRange(characters, source, index, source.length);
      break;
    }

    index += 1;
  }

  return characters.join("");
}

function attributeContents(source: string, hash: number): { contents: string; end: number } | null {
  if (source[hash] !== "#" || source[hash + 1] !== "[") return null;
  let depth = 1;
  let cursor = hash + 2;
  while (cursor < source.length && depth > 0) {
    if (source[cursor] === "[") depth += 1;
    else if (source[cursor] === "]") depth -= 1;
    cursor += 1;
  }
  if (depth !== 0) return null;
  return { contents: source.slice(hash + 2, cursor - 1), end: cursor };
}

/** Count direct Rust test and ignore attributes in lexical source code only. */
export function countRustAttributes(source: string): RustAttributeCounts {
  const code = rustCodeOnly(source);
  let tests = 0;
  let ignored = 0;
  let cursor = 0;

  while (cursor < code.length) {
    const hash = code.indexOf("#[", cursor);
    if (hash < 0) break;
    const attribute = attributeContents(code, hash);
    if (attribute === null) break;
    const contents = attribute.contents;
    if (/^\s*test\s*$/.test(contents)) tests += 1;
    else if (/^\s*tokio\s*::\s*test\s*(?:\([\s\S]*\))?\s*$/.test(contents)) tests += 1;
    else if (/^\s*ignore\b(?:\s*=\s*[\s\S]*)?$/.test(contents)) ignored += 1;
    cursor = attribute.end;
  }

  return { tests, ignored };
}
