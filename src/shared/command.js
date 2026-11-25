// Command-line splitting shared by the server console and the client.

/**
 * Split a command line into words. Double quotes group words and `\"` is a
 * literal quote inside them: `say "hello world"` -> ['say', 'hello world'].
 * @param {string} raw
 * @returns {string[]}
 */
export function splitCommand(raw) {
  const out = [];
  let cur = '';
  let quoted = false;
  let has = false;
  const text = String(raw ?? '').trim().replace(/^\//, '');
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quoted) {
      if (c === '\\' && text[i + 1] === '"') {
        cur += '"';
        i++;
      } else if (c === '"') {
        quoted = false;
      } else {
        cur += c;
      }
    } else if (c === '"') {
      quoted = true;
      has = true;
    } else if (c === ' ' || c === '\t') {
      if (has || cur) out.push(cur);
      cur = '';
      has = false;
    } else {
      cur += c;
    }
  }
  if (has || cur) out.push(cur);
  return out;
}
