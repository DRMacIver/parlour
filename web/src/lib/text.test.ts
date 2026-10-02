import { describe, expect, it } from "vitest";
import { blockquote, insertAt } from "./text";

describe("insertAt", () => {
  it("puts blocks on lines of their own", () => {
    expect(insertAt("Hello", 5, "> quote\n\n")).toEqual({ text: "Hello\n\n> quote\n\n", cursor: 16 });
    expect(insertAt("Hello\n", 6, "> q\n")).toEqual({ text: "Hello\n\n> q\n", cursor: 11 });
    expect(insertAt("", 0, "> q\n")).toEqual({ text: "> q\n", cursor: 4 });
  });

  it("inserts inline text where the cursor is", () => {
    expect(insertAt("ab", 1, "X")).toEqual({ text: "aXb", cursor: 2 });
    expect(insertAt("ab", 99, "X")).toEqual({ text: "abX", cursor: 3 });
  });
});

describe("blockquote", () => {
  it("quotes every line, keeping paragraphs", () => {
    expect(blockquote("one\n\ntwo\r\nthree\n")).toBe("> one\n>\n> two\n> three");
  });
});
