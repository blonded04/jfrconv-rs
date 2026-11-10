#!/usr/bin/env node
// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
// Executes the complete, original generated flamegraph application in a small
// deterministic DOM/canvas host. This is an interaction/draw-call comparison;
// actual browser pixels, layout and accessibility remain browser-QA concerns.
'use strict';
const fs=require('fs'),path=require('path'),vm=require('vm'),crypto=require('crypto');
const argv=process.argv.slice(2),options={strict:false,trace:false,width:1200,dpr:1,seed:0x41f1a9,output:'target/ui-parity'};
const usage=`Usage:
  node tools/ui_renderer_parity.js --java JAVA.html --rust RUST.html --strict
  node tools/ui_renderer_parity.js --corpus MANIFEST.json --output target/ui-parity --strict
Options: --seed INTEGER --width CSS_PIXELS --dpr RATIO --trace
The corpus contains {cases:[{case,format:'html'|'heatmap',files:{java,rust},flags:[]}]};
file paths are relative to the manifest. Heatmaps receive seeded copies only;
full heatmap interactions and browser pixels require browser QA.
--trace writes full frame/draw arrays; default reports contain only summaries.
`;
for(let i=0;i<argv.length;i++){
  const a=argv[i];
  if(a==='--help'){console.log(usage);process.exit(0);}
  if(a==='--strict'||a==='--trace'){options[a.slice(2)]=true;continue;}
  if(!['--java','--rust','--corpus','--output','--seed','--width','--dpr'].includes(a)||i+1>=argv.length)throw new Error('Invalid argument '+a+'\n'+usage);
  options[a.slice(2)]=argv[++i];
}
for(const key of ['seed','width','dpr'])options[key]=Number(options[key]);
if(!Number.isInteger(options.seed)||!Number.isFinite(options.width)||options.width<=0||!Number.isFinite(options.dpr)||options.dpr<=0)throw new Error('Invalid seed/viewport');
const seed=options.seed>>>0,strict=options.strict,width=options.width,dpr=options.dpr;
const output=path.resolve(options.output);fs.mkdirSync(output,{recursive:true});
let corpus,root;
if(options.corpus){const manifest=path.resolve(options.corpus);root=path.dirname(manifest);corpus=JSON.parse(fs.readFileSync(manifest,'utf8'));}
else if(options.java&&options.rust){root=process.cwd();corpus={cases:[{case:'pair',format:'html',files:{java:path.resolve(options.java),rust:path.resolve(options.rust)},flags:[]}]};}
else throw new Error(usage);
if(!Array.isArray(corpus.cases)||corpus.cases.length===0)throw new Error('Corpus must contain cases');
for(const test of corpus.cases){
  if(!/^[a-zA-Z0-9][a-zA-Z0-9_.-]*$/.test(test.case)||!['html','heatmap'].includes(test.format)||!test.files||typeof test.files.java!=='string'||typeof test.files.rust!=='string')throw new Error('Invalid corpus case');
}
if(new Set(corpus.cases.map(t=>t.case)).size!==corpus.cases.length)throw new Error('Duplicate corpus case');
const protectedInputs=new Set(corpus.cases.flatMap(t=>['java','rust'].map(i=>fs.realpathSync(path.resolve(root,t.files[i])))));
function writeOutput(file,content){
  const resolved=fs.existsSync(file)?fs.realpathSync(file):path.resolve(file);
  if(protectedInputs.has(resolved))throw new Error('Refusing to overwrite input HTML: '+file);
  fs.writeFileSync(file,content);
}
function rng() {let n=seed;return ()=>((n=(Math.imul(n,1664525)+1013904223)>>>0)/4294967296);}
function digest(x) {return crypto.createHash('sha256').update(JSON.stringify(x)).digest('hex');}
function createHost(file) {
  const html=fs.readFileSync(file,'utf8');
  const source=Array.from(html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/gi),m=>m[1]).find(s=>s.includes('function f(')&&s.includes('const cpool ='));
  if(!source) throw new Error('Missing original application script: '+file);
  const height=Number(html.match(/#canvas \{width: 100%; height: (\d+)px\}/)?.[1]);
  if(!height) throw new Error('Missing canvas height: '+file);
  const elements={},draws=[],prompts=[],selection=[],scrolls=[];
  const ctx={fillStyle:'',font:'',scale(...a){draws.push(['scale',...a]);},fillRect(...a){draws.push(['fillRect',this.fillStyle,...a]);},fillText(...a){draws.push(['fillText',this.fillStyle,...a]);}};
  for(const id of ['canvas','hl','status','inverted','search','reset','match','matchval']) elements[id]={style:{},textContent:'',firstChild:{textContent:''},title:'',offsetLeft:10,offsetTop:50};
  elements.canvas.offsetWidth=width;elements.canvas.offsetHeight=height;
  elements.canvas.getContext=(kind)=>{if(kind!=='2d')throw new Error(kind);return ctx;};
  const math=Object.create(Math);math.random=rng();
  const sandbox={Math:math,devicePixelRatio:dpr,console,event:{offsetX:0,offsetY:0,altKey:false},promptValue:null,prevented:0,
    document:{body:{style:{font:'12px Verdana, sans-serif'}},getElementById(id){if(!elements[id])throw new Error('Unexpected DOM lookup '+id);return elements[id];}},
    getSelection(){return {removeAllRanges(){selection.push('clear');},selectAllChildren(node){selection.push(['select',node.firstChild.textContent]);}};},
    prompt(...args){prompts.push(args);return sandbox.promptValue;},scroll(...args){scrolls.push(args);}};
  sandbox.window=sandbox;
  const context=vm.createContext(sandbox);
  vm.runInContext(source,context,{timeout:3000,filename:file});
  const exec=(s)=>vm.runInContext(s,context,{timeout:3000});
  function snapshot(label) {
    const app=JSON.parse(exec(`JSON.stringify({inverted,root:root&&{level:root.level,left:root.left,width:root.width,title:root.title},levels:levels.map(xs=>xs.map(f=>({...f}))),pattern:pattern&&pattern.source,nav:nav.map(f=>({level:f.level,left:f.left,width:f.width,title:f.title})),navIndex,matchval})`));
    const state={label,canvasHeight:height,headingMarkup:html.match(/<h1>(.*?)<\/h1>/s)?.[1],app,
      dom:Object.fromEntries(Object.entries(elements).map(([id,e])=>[id,{style:{...e.style},textContent:e.textContent,firstChild:e.firstChild.textContent,title:e.title,onclick:typeof e.onclick==='function'}])),
      prompts:JSON.parse(JSON.stringify(prompts)),selection:JSON.parse(JSON.stringify(selection)),scrolls:JSON.parse(JSON.stringify(scrolls)),prevented:sandbox.prevented,
      draws:JSON.parse(JSON.stringify(draws))};
    draws.length=0;return state;
  }
  return {context,sandbox,exec,snapshot,elements,html};
}
function exercise(file,sharedTarget,flags) {
  const host=createHost(file),{exec,sandbox,elements}=host,traces=[];
  traces.push(host.snapshot('initial'));
  const initial=traces[0].app;
  let target=sharedTarget;
  if(!target) {
    const frames=initial.levels.flat().filter(f=>f.level>0&&f.width>0);
    target=frames.sort((a,b)=>b.width-a.width || b.level-a.level || a.left-b.left)[0];
    if(!target)throw new Error('No meaningful nonroot interaction target: '+file);
    target={level:target.level,left:target.left,width:target.width,title:target.title};
  }
  const x=(target.left+target.width/2)/initial.root.width*width;
  const y=initial.inverted?target.level*16+8:traces[0].canvasHeight-(target.level+1)*16+8;
  sandbox.event={offsetX:x,offsetY:y,altKey:false};
  exec('canvas.onmousemove()');traces.push(host.snapshot('hover'));
  exec('canvas.ondblclick()');traces.push(host.snapshot('select-tooltip'));
  exec('canvas.onclick()');traces.push(host.snapshot('zoom'));
  exec("document.getElementById('inverted').onclick()");traces.push(host.snapshot('invert'));
  const pattern=target.title.replace(/[.*+?^${}()|[\]\\]/g,'\\$&');
  sandbox.promptValue=pattern;
  exec("document.getElementById('search').onclick()");traces.push(host.snapshot('search-button'));
  exec("window.onkeydown({key:'n',shiftKey:false})");traces.push(host.snapshot('next-match'));
  exec("window.onkeydown({key:'N',shiftKey:true})");traces.push(host.snapshot('previous-match'));
  exec("window.onkeydown({key:'Escape'})");traces.push(host.snapshot('escape-clear'));
  exec("window.onkeydown({key:'f',ctrlKey:true,preventDefault(){prevented++}})");traces.push(host.snapshot('ctrl-f'));
  exec("document.getElementById('reset').onclick()");traces.push(host.snapshot('reset-button'));
  sandbox.promptValue=null;
  exec("document.getElementById('search').onclick()");traces.push(host.snapshot('search-cancel'));
  exec('canvas.onmouseout()');traces.push(host.snapshot('mouseout'));
  const byLabel=Object.fromEntries(traces.map(t=>[t.label,t]));
  const checks={
    initial_canvas_has_draw_calls:byLabel.initial.draws.some(d=>d[0]==='fillRect'),
    initial_highlight_obeys_flag:flags.includes('--highlight')
      ?byLabel.initial.app.pattern===flags[flags.indexOf('--highlight')+1]&&byLabel.initial.draws.some(d=>d[0]==='fillRect'&&d[1]==='#ee00ee')
      :!byLabel.initial.app.pattern&&!byLabel.initial.draws.some(d=>d[0]==='fillRect'&&d[1]==='#ee00ee'),
    tooltip_contains_frame_and_sample_count:byLabel.hover.dom.canvas.title.includes(target.title)&&/samples?/.test(byLabel.hover.dom.canvas.title),
    hover_overlay_and_status_visible:byLabel.hover.dom.hl.style.display==='block'&&byLabel.hover.dom.status.style.display==='inline-block',
    double_click_selects_tooltip:byLabel['select-tooltip'].selection.some(a=>Array.isArray(a)&&a[0]==='select'),
    zoom_selects_nonroot_frame:byLabel.zoom.app.root.level>0,
    invert_changes_orientation:byLabel.invert.app.inverted!==byLabel.initial.app.inverted,
    search_calls_original_prompt:byLabel['search-button'].prompts.length===1&&byLabel['search-button'].prompts[0][0]==='Enter regexp to search:',
    search_marks_visible_frames:byLabel['search-button'].draws.some(d=>d[0]==='fillRect'&&d[1]==='#ee00ee'),
    search_shows_positive_match_percentage:byLabel['search-button'].dom.match.style.display==='inline-block'&&parseFloat(byLabel['search-button'].dom.matchval.textContent)>0,
    next_match_navigates:byLabel['next-match'].app.navIndex===0&&byLabel['next-match'].scrolls.length>0,
    previous_match_wraps_navigation:byLabel['previous-match'].app.navIndex===byLabel['previous-match'].app.nav.length-1,
    escape_clears_search:byLabel['escape-clear'].dom.match.style.display==='none'&&!byLabel['escape-clear'].app.pattern,
    ctrl_f_calls_prompt_and_prevents_browser_find:byLabel['ctrl-f'].prompts.length===2&&byLabel['ctrl-f'].prevented===1,
    reset_clears_search:byLabel['reset-button'].dom.match.style.display==='none',
    cancel_preserves_clear_state:byLabel['search-cancel'].dom.match.style.display==='none'&&byLabel['search-cancel'].prompts.length===3,
    mouseout_hides_tooltip:byLabel.mouseout.dom.hl.style.display==='none'&&byLabel.mouseout.dom.canvas.title===''
  };
  if(!Object.values(checks).every(Boolean))throw new Error(file+' interaction failure: '+JSON.stringify(checks));
  return {target,checks,traces};
}
function seededCopy(file,test,implementation) {
  const html=fs.readFileSync(file,'utf8'),directory=path.join(output,test.case,'seeded');fs.mkdirSync(directory,{recursive:true});
  const script=`<script>Math.random=(function(){let n=${seed};return function(){n=(Math.imul(n,1664525)+1013904223)>>>0;return n/4294967296;};})();</script>\n`;
  const out=path.join(directory,implementation+'.html');writeOutput(out,html.replace('<script>',script+'<script>'));return path.relative(output,out);
}
const rows=[];
for(const test of corpus.cases) {
  const seeded={};for(const impl of ['java','rust'])seeded[impl]=seededCopy(path.resolve(root,test.files[impl]),test,impl);
  if(test.format!=='html'){rows.push({case:test.case,format:test.format,seeded,renderer:'Native heatmap decoder covered separately; browser pixel/interaction check required.'});continue;}
  const java=exercise(path.resolve(root,test.files.java),undefined,test.flags||[]),rust=exercise(path.resolve(root,test.files.rust),java.target,test.flags||[]);
  const phases=java.traces.map((j,i)=>({phase:j.label,equal:JSON.stringify(j)===JSON.stringify(rust.traces[i]),java_draw_calls:j.draws.length,rust_draw_calls:rust.traces[i].draws.length,java_digest:digest(j),rust_digest:digest(rust.traces[i])}));
  const row={case:test.case,format:test.format,files:test.files,input_html_sha256:Object.fromEntries(['java','rust'].map(i=>[i,crypto.createHash('sha256').update(fs.readFileSync(path.resolve(root,test.files[i]))).digest('hex')])),seeded,checks_per_implementation:Object.keys(java.checks).length,java_checks:java.checks,rust_checks:rust.checks,phases,all_phases_identical:phases.every(p=>p.equal)};
  if(options.trace){const directory=path.join(output,test.case);fs.mkdirSync(directory,{recursive:true});for(const [impl,data] of [['java',java],['rust',rust]])writeOutput(path.join(directory,impl+'.renderer.json'),JSON.stringify(data,null,2)+'\n');}
  rows.push(row);console.log(test.case,row.all_phases_identical?'IDENTICAL':'DIFFERENCES',phases.filter(p=>!p.equal).map(p=>p.phase).join(','));
}
const result={description:'Actual generated4.1 flamegraph JavaScript executed with identical seeded Math.random and DOM/canvas mocks. Compares geometry, all draw calls, DOM updates and original event handlers; browser pixel/layout QA remains separate.',seed,canvas_width:width,devicePixelRatio:dpr,rust_binary_sha256:corpus.rust_binary_sha256,java_jar_sha256:corpus.java_jar_sha256,cases:rows};
writeOutput(path.join(output,'report.json'),JSON.stringify(result,null,2)+'\n');
const links=rows.map(r=>'<tr><td>'+r.case+'</td><td><a href="'+r.seeded.java+'">Java seeded</a></td><td><a href="'+r.seeded.rust+'">Rust seeded</a></td><td>'+(r.all_phases_identical===undefined?'heatmap browser check':r.all_phases_identical?'identical draw/events':'difference retained')+'</td></tr>');
writeOutput(path.join(output,'index.html'),'<!doctype html><meta charset="utf-8"><title>Seeded test-only UI pairs</title><h1>Seeded test-only copies</h1><p>Production HTML is unchanged. These copies set the same Math.random seed before the original application script for pixel comparison.</p><table>'+links.join('\n')+'</table><p><a href="report.json">Renderer/interaction report</a></p>');
if(strict&&rows.some(r=>r.format==='html'&&!r.all_phases_identical))process.exitCode=1;
