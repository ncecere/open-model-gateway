// The required `gateway` check must fail closed, including skipped/cancelled jobs.
const expected = ["gateway-lint", "gateway-special", "gateway-tests"];
try {
  const needs = JSON.parse(process.env.GATEWAY_NEEDS);
  if (!needs || Array.isArray(needs) || typeof needs !== "object"
      || JSON.stringify(Object.keys(needs).sort()) !== JSON.stringify(expected)
      || expected.some(name => needs[name]?.result !== "success")) {
    throw new Error("gateway requires exactly gateway-lint, gateway-tests (all four matrix legs), and gateway-special to succeed");
  }
  console.log("All gateway gates succeeded");
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
