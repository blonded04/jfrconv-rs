#!/usr/bin/env node
// Copyright The async-profiler authors
// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
// Semantic verification helper, deliberately run outside conversion benchmarks.
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const template = fs.readFileSync(path.join(__dirname, '../assets/heatmap.html'), 'utf8');
function section(from, to) {
  const begin = template.indexOf(from), end = template.indexOf(to, begin);
  if (begin < 0 || end < 0) throw Error('Missing upstream heatmap decoder');
  return template.slice(begin, end);
}
// Only the trusted, bundled upstream decoder is evaluated. Input HTML supplies
// encoded data and a JSON string table, and its JavaScript is never executed.
const decoder = Function(
  'const zoomToGroupSize=[1,50,250];\n' +
  section('class DataBuffer {', 'class HeatmapCollection extends') +
  section('function calculateMinMax(', 'function calculateInitialZoom(') +
  section('function methodName(', 'function readMethods(') +
  '\nreturn {DataBuffer,decodeExecutions,methodName};'
)();

function decode(filename) {
  const html = fs.readFileSync(filename, 'utf8');
  if (html.trim() === 'No samples found') {
    return {startMs: 0, blocks: 0, samples: 0, stacks: 0,
      digest: crypto.createHash('sha256').update('empty').digest('hex')};
  }
  function data(id) {
    const at = html.indexOf('<pre id="' + id + '">');
    if (at < 0) throw Error('Missing native heatmap data: ' + id);
    const start = html.indexOf('>', at) + 1, end = html.indexOf('</pre>', start);
    if (end < 0) throw Error('Truncated heatmap data');
    return html.slice(start, end);
  }
  const match = html.match(/let cpool = \[([\s\S]*?)\];/);
  if (!match) throw Error('Missing heatmap string table');
  const strings = JSON.parse('[' + match[1].replace(/,\s*$/, '') + ']');
  const methods = [['all', 3, 0]];
  const encoded = new decoder.DataBuffer(data('methods'));
  encoded.nextInt6();
  const methodCount = encoded.nextVarInt();
  for (let i = 0; i < methodCount; i++) {
    const cls = encoded.nextVarInt(), name = encoded.nextVarInt();
    const bci = encoded.nextInt18(), line = encoded.nextInt18(), kind = encoded.nextInt6();
    const none = bci === 0xffff && line === 0xffff;
    methods.push([decoder.methodName(strings[cls], strings[name], none ? 0 : bci, none ? 0 : line, kind),
      kind, none ? null : bci]);
  }
  const decoded = decoder.decodeExecutions(new decoder.DataBuffer(data('executionsHeatmap')));
  const ends = decoded.sampleToChunkEnds, chunks = decoded.chunksToStorage;
  const storage = decoded.storage, sizes = decoded.storageSizes;
  const pieces = new Map(), stacks = new Map(), uniqueStacks = new Set();
  function stackDigest(sample) {
    const first = sample === 0 ? 0 : ends[sample - 1], last = ends[sample];
    const key = chunks.subarray(first, last).join(',');
    let digest = stacks.get(key);
    if (digest !== undefined) return digest;
    const frames = [];
    for (let chunk = first; chunk < last; chunk++) {
      const pos = chunks[chunk];
      let piece = pieces.get(pos);
      if (piece === undefined) {
        piece = [];
        for (let p = pos + sizes[pos] - 1; p >= pos; p--) {
          const method = methods[storage[p]];
          if (method === undefined) throw Error('Invalid heatmap method reference');
          piece.push(method);
        }
        pieces.set(pos, piece);
      }
      for (const frame of piece) frames.push(frame);
    }
    digest = crypto.createHash('sha256').update(JSON.stringify(frames)).digest('hex');
    stacks.set(key, digest); uniqueStacks.add(digest);
    return digest;
  }
  const start = html.match(/let startMs = (\d+);/);
  if (!start) throw Error('Missing heatmap start time');
  const startMs = Number(start[1]);
  const digest = crypto.createHash('sha256').update('start=' + startMs + '\n');
  let previous = 0;
  for (let block = 0; block < decoded.counts.length; block++) {
    const end = decoded.counts[block], counts = new Map();
    for (let sample = previous; sample < end; sample++) {
      const stack = stackDigest(sample);
      counts.set(stack, (counts.get(stack) || 0) + 1);
    }
    digest.update(block + ':' + (end - previous) + '\n');
    for (const stack of Array.from(counts.keys()).sort()) digest.update(stack + ':' + counts.get(stack) + '\n');
    previous = end;
  }
  if (previous !== ends.length) throw Error('Heatmap block/sample totals disagree');
  return {startMs, blocks: decoded.counts.length, samples: ends.length,
    stacks: uniqueStacks.size, digest: digest.digest('hex')};
}

module.exports = decode;
if (require.main === module) {
  if (process.argv.length !== 3) { console.error('Usage: node heatmap_decode.js INPUT.html'); process.exit(2); }
  try { console.log(JSON.stringify(decode(process.argv[2]))); }
  catch (error) { console.error(error.stack || error); process.exit(1); }
}
