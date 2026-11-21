import { readFileSync, readdirSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

import ts from "typescript";

import { checkIpcArguments } from "./ipc-argument-check.mjs";

const root = fileURLToPath(new URL("../", import.meta.url));
const frontend = join(root, "frontend");
const providerMap = join(frontend, "provider-command-names.js");
const signatures = JSON.parse(readFileSync(join(root, "tools/tests/tauri-command-args.json"), "utf8"));
const manifest = Object.keys(signatures);
if (manifest.length === 0 || manifest.some((name, index) =>
  typeof name !== "string" || !/^[a-z][a-z_0-9]*$/.test(name) || (index > 0 && name <= manifest[index - 1])
)) {
  throw new Error("Die Tauri-Kommandoliste muss eindeutige, alphabetisch sortierte Namen enthalten.");
}
const registered = new Set(manifest);
const problems = [];
let checked = 0;
let resolved = 0;
let checkedPayloads = 0;
const openPayloads = [];

function* javascriptFiles(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) yield* javascriptFiles(path);
    else if (entry.isFile() && entry.name.endsWith(".js")) yield path;
  }
}

function location(source, node) {
  const { line, character } = source.getLineAndCharacterOfPosition(node.getStart(source));
  return `${relative(root, source.fileName)}:${line + 1}:${character + 1}`;
}

function checkLiteral(source, node) {
  checked += 1;
  if (!registered.has(node.text)) {
    problems.push(`${location(source, node)} Unbekanntes Tauri-Kommando „${node.text}“.`);
  }
}

function candidateCommands(source, node) {
  if (ts.isStringLiteralLike(node)) {
    checkLiteral(source, node);
    return registered.has(node.text) ? [node.text] : [];
  }
  if (ts.isConditionalExpression(node)) {
    return [...candidateCommands(source, node.whenTrue), ...candidateCommands(source, node.whenFalse)];
  }
  if (ts.isParenthesizedExpression(node)) return candidateCommands(source, node.expression);
  // Der Checker berücksichtigt Aliase und typgeprüfte Kommandotabellen.
  const type = checker.getTypeAtLocation(node);
  // Generische Provider müssen auf einen endlichen Satz registrierter Namen begrenzt sein.
  const bound = checker.getBaseConstraintOfType(type) ?? type;
  const options = bound.isUnion() ? bound.types : [bound];
  if (!options.every(option => option.isStringLiteral())) {
    problems.push(`${location(source, node)} IPC-Kommandoname ist statisch nicht auflösbar.`);
    return [];
  }
  resolved += 1;
  return options.flatMap(option => {
    if (registered.has(option.value)) return [option.value];
    problems.push(`${location(source, node)} Unbekanntes Tauri-Kommando „${option.value}“.`);
    return [];
  });
}

function checkPayload(source, call) {
  if (call.arguments.length === 0) {
    problems.push(`${location(source, call)} Tauri-Aufruf ohne Kommandonamen.`);
    return;
  }
  const commands = new Set(candidateCommands(source, call.arguments[0]));
  if (commands.size === 0) return;
  checkedPayloads += 1;
  const open = new Set();
  for (const command of commands) {
    const { issues, open: unverified } = checkIpcArguments(checker, call.arguments[1], signatures[command]);
    for (const issue of issues) problems.push(`${location(source, call)} ${command}: ${issue}`);
    for (const detail of unverified) open.add(detail);
  }
  if (open.size > 0) {
    openPayloads.push(`${location(source, call)} ${[...commands].join(" / ")} · ${[...open].sort().join(", ")}`);
  }
}

function checkProviderMap(source) {
  // Die Tabellen sind die Quelle dynamisch ausgewählter Kommandonamen.
  const tables = new Set(["providers", "syncProtocols"]);
  function visit(node) {
    if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && tables.delete(node.name.text)) {
      checkTable(node.initializer, 2);
    }
    ts.forEachChild(node, visit);
  }
  function checkTable(node, depth) {
    if (depth === 0 && node && ts.isStringLiteralLike(node)) {
      checkLiteral(source, node);
    } else if (depth > 0 && node && ts.isObjectLiteralExpression(node)) {
      for (const property of node.properties) {
        checkTable(ts.isPropertyAssignment(property) ? property.initializer : property, depth - 1);
      }
    } else {
      problems.push(`${location(source, node ?? source)} Provider-Kommandotabelle muss aus Literalen bestehen.`);
    }
  }
  visit(source);
  for (const missing of tables) {
    problems.push(`${relative(root, source.fileName)} Provider-Kommandotabelle „${missing}“ fehlt.`);
  }
}

const files = [...javascriptFiles(frontend)].sort();
const program = ts.createProgram(files, {
  allowJs: true,
  noEmit: true,
  strict: true,
  target: ts.ScriptTarget.ES2022,
  module: ts.ModuleKind.ESNext,
  types: [],
});
const checker = program.getTypeChecker();
const bridge = program.getSourceFile(join(frontend, "tauri-api.js"));
const bridgeModule = bridge && checker.getSymbolAtLocation(bridge);
const invokeSymbol = bridgeModule && checker.getExportsOfModule(bridgeModule)
  .find(symbol => symbol.getName() === "invoke");

function isInvoke(expression) {
  if (!ts.isIdentifier(expression)) return false;
  if (expression.text === "invoke") return true;
  const symbol = checker.getSymbolAtLocation(expression);
  return Boolean(symbol && symbol.flags & ts.SymbolFlags.Alias
    && checker.getAliasedSymbol(symbol) === invokeSymbol);
}

for (const path of files) {
  const source = program.getSourceFile(path);
  for (const diagnostic of program.getSyntacticDiagnostics(source)) {
    problems.push(ts.formatDiagnostic(diagnostic, {
      getCanonicalFileName: file => relative(root, file),
      getCurrentDirectory: () => root,
      getNewLine: () => "\n",
    }).trimEnd());
  }
  function visit(node) {
    if (ts.isCallExpression(node) && isInvoke(node.expression)) {
      checkPayload(source, node);
    }
    ts.forEachChild(node, visit);
  }
  visit(source);
  if (path === providerMap) checkProviderMap(source);
}

if (problems.length) {
  console.error(problems.join("\n"));
  process.exitCode = 1;
} else {
  console.log(`${checked} IPC-Namensliterale und ${resolved} indirekte Namen geprüft.`);
  console.log(`${checkedPayloads} Aufrufe auf IPC-Argumente geprüft, ${openPayloads.length} mit offenen Typen oder Feldern.`);
}

if (openPayloads.length) {
  process.exitCode = 1;
  if (process.argv.includes("--unverified")) console.error(openPayloads.join("\n"));
  else console.error("Offene Aufrufstellen zeigt node tools/check-ipc-names.mjs --unverified.");
}
