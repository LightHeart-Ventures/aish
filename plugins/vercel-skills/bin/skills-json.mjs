#!/usr/bin/env node
// vercel-skills — a lean, zero-dependency JSON-first fork of vercel-labs/skills
// (https://github.com/vercel-labs/skills, MIT). See ../REVIEW.md for the design
// rationale. This reimplements the upstream skill-discovery + SKILL.md parse
// pipeline (src/skills.ts, src/frontmatter.ts, src/types.ts) with a single
// contract: STRUCTURED JSON on stdout, no ANSI, no telemetry, no network.
//
// Requires: Node >= 18. No npm install, no node_modules, no build step.
//
// Commands:
//   list  [dir...]            JSON array of skills (name, description, path, metadata)
//   find  <query> [dir...]    same shape, filtered by query over name/description/body
//   use   <name>  [dir...]    single skill record INCLUDING its body (the prompt text)
//   catalog [dir...]          full catalog with body + contentHash for every skill
//
// Flags:
//   --include-body            include the markdown body in list/find/catalog output
//   --include-internal        include skills flagged metadata.internal: true
//   --full-depth              keep scanning subdirs even after a root SKILL.md is found
//   --max-depth <n>           recursion cap (default 6)
//   --pretty / --compact      pretty-print (default) vs single-line JSON
//   --help
//
// Default dir when none given: $AISH_SKILLS_DIR, else ~/.aish/skills, else cwd.

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, dirname, resolve, relative, sep } from 'node:path';
import { homedir } from 'node:os';
import { createHash } from 'node:crypto';

const SKIP_DIRS = new Set(['node_modules', '.git', 'dist', 'build', '__pycache__', '.next']);

// ---------------------------------------------------------------------------
// Frontmatter parsing — faithful to upstream src/frontmatter.ts (YAML delimiter
// only; NO ---js/---javascript engine, so no eval()-based RCE). Upstream leans
// on the `yaml` package; to stay zero-dependency we ship a minimal YAML-subset
// parser that covers real SKILL.md frontmatter (flat scalars, one level of
// nested maps such as `metadata:`, and simple `- ` lists). Anything it can't
// model degrades to a string rather than throwing.
// ---------------------------------------------------------------------------
function parseFrontmatter(raw) {
  const match = raw.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/);
  if (!match) return { data: {}, content: raw };
  return { data: parseYamlLite(match[1] ?? ''), content: match[2] ?? '' };
}

function stripQuotes(v) {
  const t = v.trim();
  if (
    (t.startsWith('"') && t.endsWith('"') && t.length >= 2) ||
    (t.startsWith("'") && t.endsWith("'") && t.length >= 2)
  ) {
    return t.slice(1, -1);
  }
  return t;
}

function coerceScalar(v) {
  const t = v.trim();
  if (t === '' || t === '~' || t === 'null') return null;
  if (t === 'true') return true;
  if (t === 'false') return false;
  if (/^-?\d+$/.test(t)) return Number(t);
  if (/^-?\d*\.\d+$/.test(t)) return Number(t);
  return stripQuotes(t);
}

// Minimal, indentation-aware YAML subset parser. Handles the shapes that appear
// in SKILL.md frontmatter: top-level `key: value`, nested maps (indented
// `key: value` under a bare `key:`), and inline/blocked `- item` lists.
function parseYamlLite(text) {
  // Pre-tokenize into significant lines (drop blanks + comment-only lines).
  const lines = text
    .split(/\r?\n/)
    .map((l) => ({ indent: l.length - l.replace(/^\s+/, '').length, text: l.trim() }))
    .filter((l) => l.text && !l.text.startsWith('#'));

  let i = 0;

  // Recursive-descent block parser. A block is either a map (key: value lines)
  // or a list (`- item` lines), determined by the first line's shape. Nested
  // blocks are recognised purely by deeper indentation.
  function parseBlock() {
    if (i >= lines.length) return null;
    const indent = lines[i].indent;

    if (lines[i].text.startsWith('- ')) {
      const arr = [];
      while (i < lines.length && lines[i].indent === indent && lines[i].text.startsWith('- ')) {
        arr.push(coerceScalar(lines[i].text.slice(2)));
        i++;
      }
      return arr;
    }

    const obj = {};
    while (i < lines.length && lines[i].indent === indent && !lines[i].text.startsWith('- ')) {
      const line = lines[i].text;
      const colon = line.indexOf(':');
      if (colon === -1) {
        i++;
        continue;
      }
      const key = stripQuotes(line.slice(0, colon).trim());
      const rest = line.slice(colon + 1).trim();
      i++;
      if (rest === '') {
        // nested block iff the next significant line is more deeply indented
        if (i < lines.length && lines[i].indent > indent) {
          obj[key] = parseBlock();
        } else {
          obj[key] = null;
        }
      } else {
        obj[key] = coerceScalar(rest);
      }
    }
    return obj;
  }

  const result = parseBlock();
  return result && typeof result === 'object' && !Array.isArray(result) ? result : {};
}

// ---------------------------------------------------------------------------
// Discovery — mirrors upstream findSkillDirs/discoverSkills (skills.ts).
// ---------------------------------------------------------------------------
function findSkillDirs(dir, depth, maxDepth, out) {
  if (depth > maxDepth) return;
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  let hasSkill = false;
  try {
    hasSkill = statSync(join(dir, 'SKILL.md')).isFile();
  } catch {
    /* no SKILL.md here */
  }
  if (hasSkill) out.push(dir);
  for (const e of entries) {
    if (e.isDirectory() && !SKIP_DIRS.has(e.name)) {
      findSkillDirs(join(dir, e.name), depth + 1, maxDepth, out);
    }
  }
}

function parseSkillMd(skillMdPath, opts) {
  let content;
  try {
    content = readFileSync(skillMdPath, 'utf-8');
  } catch {
    return null;
  }
  const { data, content: body } = parseFrontmatter(content);
  if (typeof data.name !== 'string' || typeof data.description !== 'string') {
    return null; // upstream: name+description are required strings
  }
  const metadata = data.metadata && typeof data.metadata === 'object' ? data.metadata : undefined;
  const isInternal = metadata && metadata.internal === true;
  if (isInternal && !opts.includeInternal) return null;

  const skill = {
    name: data.name,
    description: data.description,
    path: dirname(skillMdPath),
    metadata: metadata ?? {},
  };
  if (opts.includeBody) skill.body = body.trim();
  if (opts.includeHash) {
    skill.contentHash = createHash('sha256').update(content).digest('hex').slice(0, 16);
  }
  // stash raw for `use`
  skill.__raw = content;
  skill.__body = body.trim();
  return skill;
}

function discover(dirs, opts) {
  const skills = [];
  const seen = new Set();
  for (const base of dirs) {
    const abs = resolve(base);
    const dirsWithSkill = [];
    findSkillDirs(abs, 0, opts.maxDepth, dirsWithSkill);
    for (const d of dirsWithSkill) {
      const s = parseSkillMd(join(d, 'SKILL.md'), opts);
      if (!s) continue;
      const rel = relative(abs, d).split(sep).join('/') || '.';
      s.relativePath = rel;
      s.source = abs;
      const dedupeKey = s.path;
      if (seen.has(dedupeKey)) continue;
      seen.add(dedupeKey);
      skills.push(s);
    }
  }
  skills.sort((a, b) => a.name.localeCompare(b.name));
  return skills;
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------
function defaultDirs() {
  const env = process.env.AISH_SKILLS_DIR;
  if (env) return [env];
  const aishSkills = join(homedir(), '.aish', 'skills');
  try {
    if (statSync(aishSkills).isDirectory()) return [aishSkills];
  } catch {
    /* fall through */
  }
  return [process.cwd()];
}

function publicView(s, { includeBody }) {
  const out = {
    name: s.name,
    description: s.description,
    path: s.path,
    relativePath: s.relativePath,
    source: s.source,
    metadata: s.metadata,
  };
  if (s.contentHash) out.contentHash = s.contentHash;
  if (includeBody) out.body = s.__body;
  return out;
}

function emit(obj, opts) {
  process.stdout.write(JSON.stringify(obj, null, opts.compact ? 0 : 2) + '\n');
}

const HELP = `vercel-skills — JSON-first fork of vercel-labs/skills

USAGE
  skills-json list  [dir...] [flags]           list skills as JSON
  skills-json find  <query> [dir...] [flags]   filter skills by query
  skills-json use   <name>  [dir...] [flags]   one skill + its body (prompt)
  skills-json catalog [dir...] [flags]         full catalog (body + hash)

FLAGS
  --include-body        include markdown body in list/find/catalog
  --include-internal    include skills with metadata.internal: true
  --full-depth          scan subdirs even after a root SKILL.md
  --max-depth <n>       recursion cap (default 6)
  --compact             single-line JSON (default: pretty)
  --help

Default search dir: $AISH_SKILLS_DIR, else ~/.aish/skills, else cwd.
`;

function main(argv) {
  const args = argv.slice(2);
  if (args.length === 0 || args[0] === '--help' || args[0] === '-h') {
    process.stdout.write(HELP);
    return 0;
  }
  const cmd = args.shift();
  const opts = {
    includeBody: false,
    includeInternal: false,
    includeHash: cmd === 'catalog',
    maxDepth: 6,
    compact: false,
  };
  const positionals = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === '--include-body') opts.includeBody = true;
    else if (a === '--include-internal') opts.includeInternal = true;
    else if (a === '--full-depth') opts.fullDepth = true;
    else if (a === '--compact') opts.compact = true;
    else if (a === '--pretty') opts.compact = false;
    else if (a === '--max-depth') opts.maxDepth = Number(args[++i]) || 6;
    else if (a.startsWith('-')) {
      process.stderr.write(`vercel-skills: unknown flag ${a}\n`);
      return 2;
    } else positionals.push(a);
  }

  if (cmd === 'list' || cmd === 'catalog') {
    const dirs = positionals.length ? positionals : defaultDirs();
    if (cmd === 'catalog') opts.includeBody = true;
    const skills = discover(dirs, opts);
    emit(skills.map((s) => publicView(s, opts)), opts);
    return 0;
  }

  if (cmd === 'find') {
    if (positionals.length === 0) {
      process.stderr.write('vercel-skills: find requires <query>\n');
      return 2;
    }
    const query = positionals.shift().toLowerCase();
    const dirs = positionals.length ? positionals : defaultDirs();
    const skills = discover(dirs, opts).filter((s) => {
      const hay = `${s.name}\n${s.description}\n${s.__body}`.toLowerCase();
      return hay.includes(query);
    });
    emit(skills.map((s) => publicView(s, opts)), opts);
    return 0;
  }

  if (cmd === 'use') {
    if (positionals.length === 0) {
      process.stderr.write('vercel-skills: use requires <name>\n');
      return 2;
    }
    const name = positionals.shift().toLowerCase();
    const dirs = positionals.length ? positionals : defaultDirs();
    opts.includeBody = true;
    opts.includeHash = true;
    const skills = discover(dirs, opts);
    const hit =
      skills.find((s) => s.name.toLowerCase() === name) ||
      skills.find((s) => s.name.toLowerCase().includes(name)) ||
      skills.find((s) => s.relativePath.toLowerCase() === name);
    if (!hit) {
      process.stderr.write(`vercel-skills: no skill matching "${name}"\n`);
      emit(null, opts);
      return 1;
    }
    emit(publicView(hit, { includeBody: true }), opts);
    return 0;
  }

  process.stderr.write(`vercel-skills: unknown command "${cmd}"\n${HELP}`);
  return 2;
}

process.exit(main(process.argv));
