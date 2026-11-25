#!/usr/bin/env bun

import { $ } from "bun";

const junitPath = "target/nextest/ci/junit.xml";
const totalMaxSeconds = Number(process.env.TEST_TOTAL_MAX_S ?? "30");
const testMaxMilliseconds = Number(process.env.TEST_MAX_MS ?? "1000");

await $`rm -f ${junitPath}`;
// Build first, so the wall time measures the test run and not the compile.
await $`cargo nextest run --all-targets --no-run`;
const wallStartMilliseconds = performance.now();
const run = Bun.spawn(["cargo", "nextest", "run", "--all-targets", "--profile", "ci"], {
  stdout: "inherit",
  stderr: "inherit",
});
const runExitCode = await run.exited;

let xml: string;
try {
  xml = await Bun.file(junitPath).text();
} catch {
  console.error(`Speed gate: nextest did not produce ${junitPath}`);
  process.exit(runExitCode === 0 ? 1 : runExitCode);
}

interface TestResult {
  readonly name: string;
  readonly milliseconds: number;
}

const testResults: TestResult[] = [];
let wallTimeSeconds = 0;

wallTimeSeconds = (performance.now() - wallStartMilliseconds) / 1000;

const suitePattern = /<testsuite\b([^>]*)>/g;
for (const suiteMatch of xml.matchAll(suitePattern)) {
  const timeMatch = suiteMatch[1].match(/\btime="([^"]+)"/);
  if (wallTimeSeconds === 0 && timeMatch) {
    wallTimeSeconds += Number(timeMatch[1]);
  }
}

const testcasePattern = /<testcase\b([^>]*)\/?>(?:[\s\S]*?<\/testcase>)?/g;
for (const testcaseMatch of xml.matchAll(testcasePattern)) {
  const attributes = testcaseMatch[1];
  const nameMatch = attributes.match(/\bname="([^"]+)"/);
  const timeMatch = attributes.match(/\btime="([^"]+)"/);
  if (nameMatch && timeMatch) {
    testResults.push({
      name: nameMatch[1],
      milliseconds: Number(timeMatch[1]) * 1000,
    });
  }
}

const slowTests = testResults
  .filter((test) => test.milliseconds > testMaxMilliseconds)
  .sort((left, right) => right.milliseconds - left.milliseconds);

const rootTestsMatch = xml.match(/<testsuites\b([^>]*)>/);
const attributeValue = (name: string): number => {
  const match = rootTestsMatch?.[1].match(new RegExp(`\\b${name}=\\"([^\\"]+)\\"`));
  return match ? Number(match[1]) : 0;
};
const testCount = attributeValue("tests");
const failureCount = attributeValue("failures");
const skippedCount = attributeValue("skipped");
const passedCount = testCount - failureCount - skippedCount;
console.log(`Test results: ${passedCount} passed, ${failureCount} failed, ${skippedCount} skipped (${testCount} total)`);

if (runExitCode !== 0) {
  process.exit(runExitCode);
}

console.log(`Test wall time: ${wallTimeSeconds.toFixed(3)} s (limit ${totalMaxSeconds} s)`);
console.log(`Slowest test limit: ${testMaxMilliseconds} ms`);

let failed = false;
if (slowTests.length > 0) {
  console.error("Tests over the per-test limit:");
  for (const test of slowTests) {
    console.error(`- ${test.name}: ${test.milliseconds.toFixed(1)} ms`);
  }
  failed = true;
}
if (wallTimeSeconds > totalMaxSeconds) {
  console.error(`Total test time: ${wallTimeSeconds.toFixed(3)} s exceeds ${totalMaxSeconds} s`);
  failed = true;
}

process.exit(failed ? 1 : 0);
