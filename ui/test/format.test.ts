import { describe, expect, it } from "vitest";
import { fmtCount, fmtGb, fmtPct, historyChart, historyChartX, historyChartY, trim1 } from "../src/charts";
import { esc } from "../src/html";
import { fmtElapsed as benchElapsed, fmtVram } from "../src/views/bench";
import { fmtAge } from "../src/views/optimize";
import { fmtUptime } from "../src/views/serviceMap";

// trim1 mirrors the canonical Rust rules in crates/manager-present;
// these cases are the shared contract.
describe("trim1", () => {
  it("drops a trailing .0", () => {
    expect(trim1(16)).toBe("16");
    expect(trim1(16.0)).toBe("16");
    expect(trim1(15.96)).toBe("16");
  });
  it("keeps one decimal below 100", () => {
    expect(trim1(15.1)).toBe("15.1");
    expect(trim1(0.04)).toBe("0");
    expect(trim1(-2.25)).toBe("-2.3");
  });
  it("rounds to whole numbers at magnitude >= 100", () => {
    expect(trim1(99.94)).toBe("99.9");
    expect(trim1(100.4)).toBe("100");
    expect(trim1(1234.5)).toBe("1235");
    expect(trim1(-150.6)).toBe("-151");
  });
  it("renders non-finite values as an em dash", () => {
    expect(trim1(NaN)).toBe("—");
    expect(trim1(Infinity)).toBe("—");
    expect(trim1(-Infinity)).toBe("—");
  });
});

describe("fmtGb / fmtPct / fmtCount", () => {
  it("fmtGb applies the trim1 rule", () => {
    expect(fmtGb(15.9921875)).toBe("16");
    expect(fmtGb(23.5)).toBe("23.5");
  });
  it("fmtPct converts a 0-1 fraction", () => {
    expect(fmtPct(0.5)).toBe("50");
    expect(fmtPct(0.123)).toBe("12.3");
    expect(fmtPct(1)).toBe("100");
    expect(fmtPct(0)).toBe("0");
  });
  it("fmtCount rounds and groups thousands", () => {
    expect(fmtCount(0)).toBe("0");
    expect(fmtCount(1234)).toBe("1,234");
    expect(fmtCount(1234567.6)).toBe("1,234,568");
  });
});

describe("esc", () => {
  it("escapes the five HTML-significant characters", () => {
    expect(esc(`<a href="x" title='y'>&</a>`)).toBe("&lt;a href=&quot;x&quot; title=&#39;y&#39;&gt;&amp;&lt;/a&gt;");
  });
  it("stringifies non-string values", () => {
    expect(esc(42)).toBe("42");
    expect(esc(null)).toBe("null");
  });
});

describe("duration formatters", () => {
  it("bench fmtElapsed (ms) reads minutes and seconds, never negative", () => {
    expect(benchElapsed(0)).toBe("0 s");
    expect(benchElapsed(59_999)).toBe("59 s");
    expect(benchElapsed(125_000)).toBe("2 min 5 s");
    expect(benchElapsed(-5_000)).toBe("0 s");
  });
  it("optimize fmtAge picks the largest whole unit", () => {
    expect(fmtAge(5)).toBe("5 s");
    expect(fmtAge(60)).toBe("1 min");
    expect(fmtAge(3599)).toBe("59 min");
    expect(fmtAge(7200)).toBe("2 h");
    expect(fmtAge(3 * 86400 + 5)).toBe("3 d");
  });
  it("service-map fmtUptime is compact", () => {
    expect(fmtUptime(42)).toBe("42s");
    expect(fmtUptime(125)).toBe("2m");
    expect(fmtUptime(3600)).toBe("1h");
    expect(fmtUptime(3600 + 15 * 60)).toBe("1h15m");
  });
});

describe("fmtVram", () => {
  it("uses MiB below 1 GiB and GiB above", () => {
    expect(fmtVram(null)).toBe("—");
    expect(fmtVram(512)).toBe("512 MiB");
    expect(fmtVram(1024)).toBe("1 GiB");
    expect(fmtVram(24_576)).toBe("24 GiB");
    expect(fmtVram(1536)).toBe("1.5 GiB");
  });
});

describe("historyChart", () => {
  it("returns an empty svg with no points", () => {
    const { svg } = historyChart([], { fmtTime: String });
    expect(svg).toBe("");
  });
  it("maps the time/value extremes onto the plot edges", () => {
    const { svg, geom } = historyChart(
      [{ label: "GPU 0", color: "#f00", intervalMs: 1000, points: [{ ts: 0, v: 0 }, { ts: 1000, v: 50 }, { ts: 2000, v: 100 }] }],
      { fmtTime: String, fixedRange: [0, 100] },
    );
    expect(svg).toContain("<svg");
    expect(geom.t0).toBe(0);
    expect(geom.t1).toBe(2000);
    expect(historyChartX(geom, geom.t0)).toBe(geom.padL);
    expect(historyChartX(geom, geom.t1)).toBe(geom.w - geom.padR);
    expect(historyChartY(geom, geom.vmax)).toBeCloseTo(geom.padT);
    expect(historyChartY(geom, geom.vmin)).toBeCloseTo(geom.h - geom.padB);
  });
  it("breaks the line at a null value and at a wide time gap, never interpolating", () => {
    const pathsOf = (svg: string) => svg.match(/<path d="[^"]*" fill="none" stroke="#0f0"/g) ?? [];
    const dotsOf = (svg: string) => svg.match(/<circle [^>]*fill="#0f0"/g) ?? [];
    const contiguous = historyChart(
      [{ label: "a", color: "#0f0", intervalMs: 1000, points: [0, 1, 2, 3].map((i) => ({ ts: i * 1000, v: i })) }],
      { fmtTime: String },
    ).svg;
    expect(pathsOf(contiguous)).toHaveLength(1);
    const nullGap = historyChart(
      [{ label: "a", color: "#0f0", intervalMs: 1000, points: [{ ts: 0, v: 1 }, { ts: 1000, v: 2 }, { ts: 2000, v: null }, { ts: 3000, v: 3 }, { ts: 4000, v: 4 }] }],
      { fmtTime: String },
    ).svg;
    expect(pathsOf(nullGap)).toHaveLength(2);
    // A lone sample after a >3x-interval gap is drawn as a dot.
    const timeGap = historyChart(
      [{ label: "a", color: "#0f0", intervalMs: 1000, points: [{ ts: 0, v: 1 }, { ts: 1000, v: 2 }, { ts: 10_000, v: 3 }] }],
      { fmtTime: String },
    ).svg;
    expect(pathsOf(timeGap)).toHaveLength(1);
    expect(dotsOf(timeGap)).toHaveLength(1);
  });
});
