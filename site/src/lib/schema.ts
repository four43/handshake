// Turns the JSON Schemas written by `cargo test schemas_are_current` (site/schemas/) into
// rows the reference pages render. Only the shapes schemars emits for our types are handled.

export type Schema = {
  $ref?: string;
  $defs?: Record<string, Schema>;
  type?: string | string[];
  format?: string;
  const?: unknown;
  default?: unknown;
  description?: string;
  properties?: Record<string, Schema>;
  required?: string[];
  items?: Schema;
  additionalProperties?: Schema | boolean;
  oneOf?: Schema[];
  anyOf?: Schema[];
};

export type Field = { name: string; type: string; required: boolean; default?: string; description: string };

export function resolve(root: Schema, s: Schema): Schema {
  const some = s.anyOf?.filter(v => v.type !== 'null');
  if (some?.length === 1 && some[0].$ref) return { ...resolve(root, some[0]), description: s.description };
  if (!s.$ref) return s;
  const target = root.$defs?.[s.$ref.replace('#/$defs/', '')] ?? {};
  // A $ref can carry its own description (from the field's doc comment); it wins over the type's.
  return { ...target, description: s.description ?? target.description };
}

/** A short type label: `string`, `integer`, `string[]`, `"all" | "none"`, `JSON`. */
export function typeLabel(root: Schema, s: Schema): string {
  s = resolve(root, s);
  const consts = s.oneOf?.map(v => v.const).filter(v => v !== undefined);
  if (consts?.length) return consts.map(v => JSON.stringify(v)).join(' | ');
  const variants = s.anyOf?.filter(v => v.type !== 'null');
  if (variants?.length === 1) return typeLabel(root, variants[0]);
  const types = ([] as string[]).concat(s.type ?? []).filter(t => t !== 'null');
  if (!types.length) return 'JSON';
  return types
    .map(t => (t === 'array' && s.items ? `${typeLabel(root, s.items)}[]` : t === 'object' ? 'table' : t))
    .join(' | ');
}

function isOptional(root: Schema, s: Schema): boolean {
  if (s.anyOf?.some(v => v.type === 'null')) return true;
  return ([] as string[]).concat(resolve(root, s).type ?? []).includes('null');
}

export function fields(root: Schema, s: Schema): Field[] {
  s = resolve(root, s);
  const required = new Set(s.required ?? []);
  return Object.entries(s.properties ?? {}).map(([name, prop]) => {
    const def = prop.default;
    return {
      name,
      type: typeLabel(root, prop),
      required: required.has(name) && !isOptional(root, prop),
      default: def === undefined || def === null ? undefined : JSON.stringify(def),
      description: resolve(root, prop).description ?? '',
    };
  });
}

/** Variants of an internally tagged enum (`#[serde(tag = "t")]`). */
export function variants(root: Schema, tag = 't') {
  return (root.oneOf ?? []).map(v => ({
    name: String(v.properties?.[tag]?.const ?? ''),
    description: v.description ?? '',
    fields: fields(root, v).filter(f => f.name !== tag),
  }));
}

const escape = (s: string) => s.replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);

/** Doc comments use Markdown inline code only; render that and escape the rest. */
export const inline = (s: string) => escape(s).replace(/`([^`]+)`/g, '<code>$1</code>');
