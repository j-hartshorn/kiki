
import { readFileSync } from 'node:fs';
const { parseColor, colorToHex } = await import(process.argv[2]);
for (const f of process.argv.slice(3)) {
  const t = JSON.parse(readFileSync(f, 'utf8'));
  const res = (v) => { while (typeof v === 'string' && t.vars && v in t.vars) v = t.vars[v]; return v; };
  const out = {};
  for (const [k, v] of Object.entries(t.colors)) { const r = res(v); out[k] = r === '' ? '' : colorToHex(parseColor(r)); }
  out['export.cardBg'] = colorToHex(parseColor(res(t.export.cardBg)));
  console.log(t.name, JSON.stringify(out));
}
