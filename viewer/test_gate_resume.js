const assert = require('assert'), fs = require('fs'), os = require('os'), path = require('path'), vm = require('vm');
const source = fs.readFileSync(require.resolve('./server'), 'utf8');
const { readTopLevelScalars, createWindowedScalarReader } = require('./protocol_hints');
const root=fs.mkdtempSync(path.join(os.tmpdir(),'trellis-gate-resume-'));
try {
  const crypto=require('crypto');
  const context=vm.createContext({fs,path,Date,crypto,readTopLevelScalars,createWindowedScalarReader,console,
    readPauseConfig:()=>({gate_park_after_minutes:0}),runtimeRootForProject:()=>root,
    pauseStatusCached:()=>({state:'running'}),invalidatePauseStatus:()=>{},
    runPauseScript:()=>{ context.arms++; },arms:0});
  vm.runInContext(source.slice(source.indexOf('const GATE_PARK_DEFAULT_MINUTES'),source.indexOf('// The floor has to be enforced')),context);
  const state={stage:'HumanGate',cycle:72,gate_kind:'AssumptionReview',in_flight_request:{id:363,kind:'HumanGate',cycle:72}};
  const write=()=>fs.writeFileSync(path.join(root,'protocol_state.json'),JSON.stringify(state));write();
  const signal=path.join(root,'human_gate_response.json');
  const marker=path.join(root,'gate_decision_seen.json');
  const original=JSON.stringify({choice:'approve',cycle:72,request_id:363});
  fs.writeFileSync(signal,original);
  context.expected=crypto.createHash('sha256').update(original).digest('hex');
  assert.strictEqual(vm.runInContext('rememberGateDecision(runtimeRootForProject(), "0".repeat(64))',context),false);
  assert.strictEqual(fs.existsSync(marker),false);
  assert.strictEqual(vm.runInContext('rememberGateDecision(runtimeRootForProject(), expected)',context),true);
  assert.strictEqual(fs.readFileSync(signal,'utf8'),original,'parking receipt leaves native signal exact');
  assert.strictEqual(JSON.parse(fs.readFileSync(marker)).signal_sha256,context.expected);
  context.binding=vm.runInContext('captureGateDecisionBinding(runtimeRootForProject(), "approve")',context);
  fs.unlinkSync(signal);fs.unlinkSync(marker);
  context.expected=crypto.createHash('sha256').update(JSON.stringify(context.binding,null,2)+'\n').digest('hex');
  assert.strictEqual(vm.runInContext('rememberGateDecision(runtimeRootForProject(), expected, binding)',context),true,
    'adapter-confirmed consumed decision remains bound before its return');
  fs.unlinkSync(marker);
  assert.strictEqual(vm.runInContext('rememberGateDecision(runtimeRootForProject(), "0".repeat(64), binding)',context),false);
  assert.strictEqual(fs.existsSync(marker),false,'changed gate or saved bytes cannot create a pending receipt');
  for(const choice of ['approve','feedback']) {
    fs.writeFileSync(path.join(root,'human_gate_response.json'),JSON.stringify({choice,cycle:72,request_id:363}));
    assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason',context),'decision_pending');
    fs.unlinkSync(path.join(root,'human_gate_response.json'));
    assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason',context),'decision_pending',
      'consumed signal must not repark before the native gate checkpoint');
  }
  assert.strictEqual(context.arms,0);
  // A fresh viewer instance must recover the pending decision from disk.
  const restarted=vm.createContext({fs,path,Date,crypto});
  vm.runInContext(source.slice(source.indexOf('function readGateDecision('),source.indexOf('function gateSeenPath(')),restarted);
  restarted.root=root;
  assert.strictEqual(vm.runInContext('gateHasSavedDecision(root,{cycle:72,request_id:363})',restarted),true);
  vm.runInContext('gateOpenMinutes(runtimeRootForProject(), "None")',context);
  assert.strictEqual(fs.existsSync(marker),false,'durable gate exit clears parking receipt');
  state.in_flight_request.id=364;write();
  assert.strictEqual(vm.runInContext('evaluateGatePark({}).armed',context),true,'same-cycle old request does not suppress parking');
  const seen=JSON.parse(fs.readFileSync(path.join(root,'gate_seen.json')));assert.strictEqual(seen.request_id,364);
  state.cycle=73;state.in_flight_request.cycle=73;write();
  vm.runInContext('evaluateGatePark({})',context);
  assert.strictEqual(JSON.parse(fs.readFileSync(path.join(root,'gate_seen.json'))).cycle,73);
  // Production gate363 carries a 415 KiB frozen contract. Stream scalar
  // identity across both sides of that payload without buffering the request.
  state.in_flight_request={payload:{text:'x'.repeat(500000), historical:{id:999,cycle:1,kind:'Worker'}},
    id:364,kind:'HumanGate',cycle:73};write();
  assert.strictEqual(vm.runInContext('evaluateGatePark({}).armed',context),true);
  state.in_flight_request.cycle=72;write();
  assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason',context),'gate_unknown');
  fs.writeFileSync(path.join(root,'protocol_state.json'),JSON.stringify({dispatch:{request:state},gate_kind:'AssumptionReview'}));
  assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason',context),'gate_unknown');
  // The identical native gate can outlive a process-down pause. A fresh
  // supervisor starts a new live interval even if the viewer was offline for
  // the whole pause; a viewer restart must keep the same process interval.
  {
    const dir=fs.mkdtempSync(path.join(os.tmpdir(),'trellis-gate-live-age-'));
    try {
      const fixedNow=Date.UTC(2027,0,1,0,0,0);
      class FixedDate extends Date {static now(){return fixedNow;}}
      fs.writeFileSync(path.join(dir,'protocol_state.json'),JSON.stringify({stage:'HumanGate',cycle:72,
        gate_kind:'Advance',in_flight_request:{id:363,kind:'HumanGate',cycle:72}}));
      const old={gate_kind:'Advance',cycle:72,request_id:363,
        first_seen_epoch:Math.floor(fixedNow/1000)-3*60*60};
      fs.writeFileSync(path.join(dir,'gate_seen.json'),JSON.stringify(old));
      let state='paused', pid=123;
      const make=()=>{
        const c=vm.createContext({fs,path,Date:FixedDate,crypto,readTopLevelScalars,createWindowedScalarReader,console,
          readPauseConfig:()=>({gate_park_after_minutes:120}),runtimeRootForProject:()=>dir,
          pauseStatusCached:()=>({state,wrapper_pid:pid}),invalidatePauseStatus:()=>{},
          runPauseScript:()=>{c.arms++;},arms:0});
        vm.runInContext(source.slice(source.indexOf('const GATE_PARK_DEFAULT_MINUTES'),source.indexOf('// The floor has to be enforced')),c);
        return c;
      };
      let c=make();
      assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason',c),'state_paused');
      assert.deepStrictEqual(JSON.parse(fs.readFileSync(path.join(dir,'gate_seen.json'))),old);
      state='running';
      vm.runInContext('gateSupervisorStartEpoch = () => Math.floor(Date.now()/1000)-60*60',c);
      assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason',c),'not_yet');
      assert.strictEqual(c.arms,0,'a sweep at the resume boundary must not re-park');
      assert.deepStrictEqual(JSON.parse(fs.readFileSync(path.join(dir,'gate_seen.json'))),old);
      c=make(); // viewer restart while this supervisor is still up
      vm.runInContext('gateSupervisorStartEpoch = () => Math.floor(Date.now()/1000)-60*60',c);
      assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason',c),'not_yet');
      vm.runInContext('gateSupervisorStartEpoch = () => Math.floor(Date.now()/1000)-4*60*60',c);
      assert.strictEqual(vm.runInContext('evaluateGatePark({}).armed',c),true,
        'an uninterrupted old supervisor must still park an overdue gate');
      // A failed identity probe leaves the older disk timer authoritative.
      vm.runInContext('gateSupervisorStartEpoch = () => null',c);
      assert.strictEqual(vm.runInContext('evaluateGatePark({}).armed',c),true);
      assert.strictEqual(c.arms,2);
      // A matching saved decision continues to suppress parking independently.
      fs.writeFileSync(path.join(dir,'human_gate_response.json'),JSON.stringify({choice:'approve',cycle:72,request_id:363}));
      assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason',c),'decision_pending');
    } finally {fs.rmSync(dir,{recursive:true,force:true});}
  }
  {
    const runtime='/tmp/trellis-gate-runtime';
    const fixedNow=Date.UTC(2027,0,1,0,0,0);
    class FixedDate extends Date {static now(){return fixedNow;}}
    const c=vm.createContext({Number,Date:FixedDate,process,execFileSync:(_cmd,args,options)=>{
      assert.strictEqual(args[2],'321');
      assert.strictEqual(options.env.LC_ALL,'C');
      assert.strictEqual(options.env.PATH,process.env.PATH);
      return `Thu Oct 29 12:00:00 2026 bash /source/scripts/trellis.sh run ${runtime}\n`;
    }});
    vm.runInContext(source.slice(source.indexOf('function gateSupervisorStartEpoch('),source.indexOf('function evaluateGatePark(')),c);
    c.runtime=runtime;
    assert.strictEqual(vm.runInContext('gateSupervisorStartEpoch(0,runtime)',c),null);
    assert.strictEqual(vm.runInContext('gateSupervisorStartEpoch("321",runtime)',c),null);
    assert(Number.isFinite(vm.runInContext('gateSupervisorStartEpoch(321,runtime)',c)));
    c.execFileSync=()=>`Thu Oct 29 12:00:00 2026 python -m trellis.dispatch resume ${runtime}\n`;
    assert(Number.isFinite(vm.runInContext('gateSupervisorStartEpoch(321,runtime)',c)));
    c.execFileSync=()=>`Thu Oct 29 12:00:00 2026 bash /other/scripts/trellis.sh run /other/runtime\n`;
    assert.strictEqual(vm.runInContext('gateSupervisorStartEpoch(321,runtime)',c),null);
    c.execFileSync=()=>`Thu Oct 29 12:00:00 2026 bash /source/scripts/trellis.sh run ${runtime}-other\n`;
    assert.strictEqual(vm.runInContext('gateSupervisorStartEpoch(321,runtime)',c),null);
    c.execFileSync=()=>{throw Error('ps unavailable');};
    assert.strictEqual(vm.runInContext('gateSupervisorStartEpoch(321,runtime)',c),null);
  }
  const start=source.indexOf('function handleFeedbackPost(');
  const feedback=source.slice(start,source.indexOf('  JSON.parse(fs.readFileSync',start))+'}\n';
  // Exercise the real Python adapter's serialized decision, with consumption
  // before Node receives its return value from an already-running coordinator.
  for (const choice of ['approve', 'feedback']) {
    const dir=fs.mkdtempSync(path.join(os.tmpdir(),'trellis-gate-consumed-'));
    try {
      fs.writeFileSync(path.join(dir,'protocol_state.json'),JSON.stringify({stage:'HumanGate',cycle:72,
        gate_kind:'AssumptionReview',in_flight_request:{id:363,kind:'HumanGate',cycle:72}}));
      let result;
      const c=vm.createContext({fs,path,Date,crypto,readTopLevelScalars,createWindowedScalarReader,console,
        resolveRepoPath:()=>({repoType:'trellis'}),runtimeRootForProject:()=>dir,
        readPauseConfig:()=>({gate_park_after_minutes:0}),pauseStatusRaw:()=>({}),
        pauseStatusCached:()=>({state:'running'}),invalidatePauseStatus:()=>{},
        runPauseScript:()=>{throw Error('a saved running-gate decision must not park or restart');},
        trellisAdapter:()=>{
          const py='import json,sys\nfrom pathlib import Path\nfrom types import SimpleNamespace\nfrom trellis import viewer_adapter as v\nr=Path(sys.argv[1])\nv._load_runtime_info=lambda _:SimpleNamespace(root=r,protocol_state_path=r/"protocol_state.json")\nprint(json.dumps(v._feedback_post(r,action=sys.argv[2],feedback="kept")))';
          const value=JSON.parse(require('child_process').execFileSync('/usr/bin/python3',['-c',py,dir,choice],
            {encoding:'utf8',env:{...process.env,PYTHONDONTWRITEBYTECODE:'1',PYTHONPATH:path.resolve(__dirname,'..')}}));
          fs.unlinkSync(path.join(dir,'human_gate_response.json'));
          return value;
        }});
      vm.runInContext(source.slice(source.indexOf('const GATE_PARK_DEFAULT_MINUTES'),source.indexOf('// The floor has to be enforced')),c);
      vm.runInContext(feedback,c);
      c.req={body:{action:choice,feedback:'kept'}};c.res={json:v=>result=v,status:()=>{throw Error('unexpected adapter failure');}};
      vm.runInContext('handleFeedbackPost(req,res,"run")',c);
      assert.strictEqual(result.ok,true);
      assert.strictEqual(fs.existsSync(path.join(dir,'human_gate_response.json')),false);
      assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason',c),'decision_pending');
      assert.strictEqual(JSON.parse(fs.readFileSync(path.join(dir,'gate_decision_seen.json'))).signal_sha256,result.gate_response_sha256);
    } finally {fs.rmSync(dir,{recursive:true,force:true});}
  }
  for(const choice of ['approve','feedback']) for(const kind of ['gate_parked','deployment','quota_budget']) for(const fail of [false,true]) {
    let calls=0,result;
    const c=vm.createContext({resolveRepoPath:()=>({repoType:'trellis'}),
      trellisAdapter:()=>({ok:true,gate_response_sha256:'a'.repeat(64),message:'saved'}),
      runtimeRootForProject:()=>root,captureGateDecisionBinding:()=>null,rememberGateDecision:()=>true,
      pauseStatusRaw:()=>({request:{kind}}),invalidatePauseStatus:()=>{},
      runPauseScript:(p,args,timeout,flags)=>{calls++;assert.strictEqual(flags[0],'--gate-response');if(fail)throw Error('failed start');}});
    vm.runInContext(feedback,c);
    c.req={body:{action:choice,feedback:'text'}};c.res={json:v=>result=v,status:()=>{throw Error('unexpected failure')}};
    vm.runInContext('handleFeedbackPost(req,res,"run")',c);
    assert.strictEqual(calls,kind==='gate_parked'?1:0);
    if(kind==='gate_parked')assert.strictEqual(result.restart_confirmed,!fail);
  }
  {
    let result;
    const c=vm.createContext({resolveRepoPath:()=>({repoType:'trellis'}),
      trellisAdapter:()=>({ok:true,gate_response_sha256:'a'.repeat(64)}),
      runtimeRootForProject:()=>root,captureGateDecisionBinding:()=>null,rememberGateDecision:()=>false,
      pauseStatusRaw:()=>({request:{kind:'gate_parked'}}),invalidatePauseStatus:()=>{},
      runPauseScript:()=>{throw Error('resume must wait for the bound parking receipt');}});
    vm.runInContext(feedback,c);
    c.req={body:{action:'approve'}};c.res={json:v=>result=v,status:()=>{throw Error('unexpected failure')}};
    vm.runInContext('handleFeedbackPost(req,res,"run")',c);
    assert.strictEqual(result.restart_confirmed,false);
  }
  console.log('gate decision resume and timer tests passed');
} finally { fs.rmSync(root,{recursive:true,force:true}); }

// Exercise the authenticated route's actual config implementation, preserving
// each independent setting and rejecting invalid values before any write.
{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'trellis-pause-config-'));
  try {
    const context=vm.createContext({fs,path,PAUSE_DEFAULT_THRESHOLD:5,GATE_PARK_DEFAULT_MINUTES:120,
      PAUSE_THRESHOLD_CHOICES:[null,5,10,25], evaluateBudgetPause:()=>({armed:false}),
      evaluateGatePark:()=>({armed:false}), pauseStateResponse:()=>({}), project:{stateDir:dir}});
    vm.runInContext(source.slice(source.indexOf('function pauseConfigPath('),source.indexOf('// `flags` follow')),context);
    vm.runInContext(source.slice(source.indexOf('function handlePauseConfig('),source.indexOf("registerControlRoute('pause/config'")),context);
    const apply=body=>{context.body=body;return vm.runInContext('handlePauseConfig(project,body)',context);};
    const config=()=>JSON.parse(fs.readFileSync(path.join(dir,'pause_config.json')));
    apply({budget_threshold_pct:null,gate_park_after_minutes:120});
    apply({gate_park_after_minutes:null});
    assert.deepStrictEqual(config(),{budget_threshold_pct:null,gate_park_after_minutes:null});
    apply({gate_park_after_minutes:120});
    assert.deepStrictEqual(config(),{budget_threshold_pct:null,gate_park_after_minutes:120});
    apply({budget_threshold_pct:10});
    assert.strictEqual(config().gate_park_after_minutes,120);
    for(const value of [0,-1,true,{},[], 'bad']) {
      const before=fs.readFileSync(path.join(dir,'pause_config.json'),'utf8');
      assert.throws(()=>apply({gate_park_after_minutes:value}));
      assert.strictEqual(fs.readFileSync(path.join(dir,'pause_config.json'),'utf8'),before);
    }
    assert.throws(()=>apply({}));
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
  console.log('independent parking-timeout control tests passed');
}

(async () => {
  const html=fs.readFileSync(path.join(__dirname,'public/index.html'),'utf8');
  const snippet=html.slice(html.indexOf('const PAUSE_THRESHOLD_LABELS'),html.indexOf('async function renderUsageDetail'));
  const posts=[];
  function element(tag) {return {tag,style:{},children:[],appendChild(child){this.children.push(child);}};}
  const c=vm.createContext({document:{createElement:element},
    pauseState:{config:{budget_threshold_pct:null,gate_park_after_minutes:120},state:'paused'},
    controlPath:p=>p,controlHeaders:h=>({...h,Authorization:'test-authorized'}),renderPauseBanner:()=>{},
    fetch:async (path,request)=>{posts.push({path,...request});return {ok:true,json:async()=>({config:{},state:'paused'})};}});
  vm.runInContext(snippet,c);
  const wrap=vm.runInContext('buildBudgetPauseControl()',c);
  const [budget,park]=wrap.children.filter(x=>x.tag==='select');
  assert.strictEqual(budget.value,'never');assert.strictEqual(park.value,'120');
  park.value='never';await park.onchange();
  park.value='120';await park.onchange();
  assert.deepStrictEqual(posts.map(x=>JSON.parse(x.body)),[{gate_park_after_minutes:null},{gate_park_after_minutes:120}]);
  assert(posts.every(x=>x.path==='pause/config'&&x.headers.Authorization==='test-authorized'));
  assert.strictEqual(budget.value,'never');
  console.log('rendered parking-timeout control passed');
})().catch(error=>{console.error(error);process.exitCode=1;});
