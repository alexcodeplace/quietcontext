import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (path: string) => readFileSync(join(root, path), "utf8");
const sha256 = (path: string) => createHash("sha256").update(readFileSync(join(root, path))).digest("hex");

describe("native third-party license provenance", () => {
  test("preserves exact Fewtok and source-time RTK license texts", () => {
    expect(sha256("native/qc/LICENSES/Fewtok-MIT.txt")).toBe(
      "53da06007a13cacb63bcd1788274a92901d734cfcb24053a7ad2c67155bb31e8",
    );
    expect(sha256("native/qc/LICENSES/Apache-2.0.txt")).toBe(
      "4044ade9c21d8b084d3d16a03375cf3b7e166b946a327bb37a3fbbdb53287cfd",
    );
    expect(read("native/qc/LICENSES/Fewtok-MIT.txt")).toContain(
      "Copyright (c) 2026 fewtok contributors",
    );
    const rtkLicense = read("native/qc/LICENSES/Apache-2.0.txt");
    expect(rtkLicense).toContain("Apache License");
    expect(rtkLicense).toContain("Copyright 2024 rtk-ai and rtk-ai Labs");
    expect(rtkLicense).not.toContain("Copyright [yyyy] [name of copyright owner]");
  });

  test("ships explicit notices for the Fewtok and RTK portions", () => {
    const notice = read("native/qc/NOTICE");
    expect(notice).toContain("Fewtok");
    expect(notice).toContain("License: MIT");
    expect(notice).toContain("RTK (Rust Token Killer)");
    expect(notice).toContain("Copyright 2024 rtk-ai and rtk-ai Labs");
    expect(notice).toContain("License: Apache License, Version 2.0");

    const provenance = read("native/qc/PROVENANCE.md");
    expect(provenance).toContain("LICENSES/Fewtok-MIT.txt");
    expect(provenance).toContain("LICENSES/Apache-2.0.txt");

    const pkg = JSON.parse(read("package.json"));
    expect(pkg.files).toContain("native/qc/NOTICE");
    expect(pkg.files).toContain("native/qc/PROVENANCE.md");
    expect(pkg.files).toContain("native/qc/LICENSES");
  });
});
