import { defineConfig } from "vitest/config";

// Unit tests for the UI's pure logic (formatters, parsers, payload
// normalisers). Tests live in ui/test/ — outside src/, so the smoke
// script's command scan and the production tsc pass never see them —
// and run in the plain Node environment: the modules under test touch
// `window` only inside function bodies, which the tests stub.
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    environment: "node",
  },
});
