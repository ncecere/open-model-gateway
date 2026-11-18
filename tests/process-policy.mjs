// Syntax-level authority inventory, not a JavaScript sandbox. Fixture strings and
// comments are data; the parser never evaluates or imports the inspected source.
import ts from "typescript";

const trusted = new Set(["bash-preflight.mjs", "bash-preflight.test.mjs", "process.mjs"]);
const forbidden = new Set(["child_process", "node:child_process", "module", "node:module"]);
const escapeHatches = new Set(["binding", "_linkedBinding", "dlopen", "getBuiltinModule"]);
function referencesRequire(node) {
  if (!ts.isIdentifier(node) || node.text !== "require" || ts.isInTypeQuery(node)) return false;
  const parent = node.parent;
  if (ts.isPropertyAccessExpression(parent) && parent.name === node) {
    const owner = ts.skipOuterExpressions(parent.expression);
    return ts.isIdentifier(owner) && ["module", "global", "globalThis"].includes(owner.text);
  }
  return ts.isShorthandPropertyAssignment(parent) || ts.isExpressionNode(node);
}

export function processViolations(name, text) {
  const source = ts.createSourceFile(name, text, ts.ScriptTarget.Latest, true,
    /\.tsx$/i.test(name) ? ts.ScriptKind.TSX : /\.[cm]?ts$/i.test(name) ? ts.ScriptKind.TS : /\.jsx$/i.test(name) ? ts.ScriptKind.JSX : ts.ScriptKind.JS);
  const found = source.parseDiagnostics.map(d => `${name}: ${ts.flattenDiagnosticMessageText(d.messageText, " ")}`);
  const report = (node, reason) => found.push(`${name}:${source.getLineAndCharacterOfPosition(node.getStart(source)).line + 1}: ${reason}`);
  function visit(node) {
    if (ts.isStringLiteralLike(node) && forbidden.has(node.text)) {
      const parent = node.parent;
      const load = ts.isImportDeclaration(parent) || ts.isExportDeclaration(parent) || ts.isExternalModuleReference(parent) ||
        (ts.isCallExpression(parent) && (parent.expression.kind === ts.SyntaxKind.ImportKeyword ||
          (ts.isIdentifier(parent.expression) && parent.expression.text === "require")));
      if (load && !trusted.has(name)) report(node, "use tests/process.mjs instead of raw process/module authority");
    }
    if (!trusted.has(name)) {
      if (ts.isImportDeclaration(node) && ts.isStringLiteralLike(node.moduleSpecifier) && /(?:^|\/)bash-preflight\.mjs$/.test(node.moduleSpecifier.text)) {
        const bindings = node.importClause?.namedBindings;
        if (node.importClause?.name || !bindings || !ts.isNamedImports(bindings) ||
            bindings.elements.some(item => !["runBash", "bashPath"].includes((item.propertyName ?? item.name).text))) report(node, "only bashPath/runBash may be imported from the Bash preflight");
      }
      if (ts.isExportDeclaration(node) && node.moduleSpecifier && /bash-preflight\.mjs$/.test(node.moduleSpecifier.text ?? "")) report(node, "do not re-export Bash preflight authority");
      if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword &&
          node.arguments[0] && ts.isStringLiteralLike(node.arguments[0]) && /bash-preflight\.mjs$/.test(node.arguments[0].text)) report(node, "import only named bashPath/runBash from the Bash preflight");
      if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword &&
          (node.arguments.length !== 1 || !ts.isStringLiteralLike(node.arguments[0]))) report(node, "dynamic imports require a literal module specifier");
      const key = ts.isPropertyAccessExpression(node) ? node.name.text :
        ts.isElementAccessExpression(node) && ts.isStringLiteralLike(node.argumentExpression) ? node.argumentExpression.text : undefined;
      if (escapeHatches.has(key) && (key !== "binding" || node.expression?.getText(source) === "process")) report(node, "native process loading is outside the test process API");
      // CommonJS loading can read JavaScript from extensions otherwise treated as
      // data. Tests use ESM; keep require (including aliases) out of executable code.
      if (referencesRequire(node)) report(node, "CommonJS loading is outside the test process API");
    }
    ts.forEachChild(node, visit);
  }
  visit(source);
  return found;
}
