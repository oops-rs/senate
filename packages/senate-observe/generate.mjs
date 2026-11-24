import { readFileSync, writeFileSync } from "node:fs";

const schemas = ["snapshot", "event", "reset", "error"].map((name) => JSON.parse(
  readFileSync(new URL(`../../docs/observe/v1/${name}.schema.json`, import.meta.url), "utf8"),
));
const definitions = new Map();
for (const schema of schemas) {
  for (const [name, definition] of Object.entries(schema.$defs ?? {})) {
    const before = definitions.get(name);
    if (before && JSON.stringify(before) !== JSON.stringify(definition)) {
      throw new Error(`Conflicting Observe schema definition ${name}`);
    }
    definitions.set(name, definition);
  }
}
for (const schema of schemas) definitions.set(schema.title, schema);

function type(schema) {
  if (typeof schema !== "object") return schema === false ? "never" : "unknown";
  if (schema.$ref) return schema.$ref.split("/").at(-1);
  if ("const" in schema) return JSON.stringify(schema.const);
  if (schema.enum) return schema.enum.map((value) => JSON.stringify(value)).join(" | ");
  const union = schema.oneOf ?? schema.anyOf;
  if (union) {
    const choices = union.map(type).join(" | ");
    if (!schema.properties) return choices;
    const { oneOf: _oneOf, anyOf: _anyOf, ...base } = schema;
    return `${type(base)} & (${choices})`;
  }
  if (schema.allOf) return schema.allOf.map((part) => `(${type(part)})`).join(" & ");
  if (Array.isArray(schema.type)) return schema.type.map((kind) => type({ ...schema, type: kind })).join(" | ");
  switch (schema.type) {
    case "object": {
      if (!schema.properties) return `Record<string, ${type(schema.additionalProperties ?? true)}>`;
      const properties = Object.entries(schema.properties).map(([name, field]) =>
        `  ${JSON.stringify(name)}${schema.required?.includes(name) ? "" : "?"}: ${type(field)};`,
      );
      return `{\n${properties.join("\n")}\n}`;
    }
    case "integer":
    case "number": return "number";
    case "array": return `Array<${type(schema.items ?? true)}>`;
    case "string":
      return schema.pattern?.startsWith("^mcp:") ? "`mcp:${string}`" : "string";
    case "null":
    case "boolean": return schema.type;
    default: return "unknown";
  }
}
const output = "// Generated from the Rust Observe schemas. Run npm run generate.\n\n" +
  [...definitions].sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)
    .map(([name, definition]) => `export type ${name} = ${type(definition)};\n`).join("\n");
const path = new URL("./src/wire.d.ts", import.meta.url);
if (process.argv.includes("--check")) {
  if (readFileSync(path, "utf8") !== output) throw new Error("Observe TypeScript definitions are stale");
} else {
  writeFileSync(path, output);
}
