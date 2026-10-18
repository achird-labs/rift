// Regenerates csv_parse_7_0_2.jsonl, the oracle lookup.rs's parser is tested against (issue #1566):
//   node gen_csv_parse_oracle.js <path to csv-parse 7.0.2, e.g. mountebank/node_modules/csv-parse> 1000 > csv_parse_7_0_2.jsonl
// Each line is a seeded random input and what csv-parse's streaming parser emits before its first
// error (Mountebank 2.9.5 resolves a lookup from those rows only), plus that error's code.
const {parse}=require(process.argv[2]); const N=+process.argv[3];
const atoms=['a','b','"','""',',','\n','\r\n','\r',' ','x"y','"q,r"','"a""b"','||','é'];
let seed=7; const rnd=()=>{seed=(seed*1103515245+12345)%2147483648; return seed;};
const cases=[]; for(let k=0;k<N;k++){let t='';const L=1+rnd()%10;for(let j=0;j<L;j++)t+=atoms[rnd()%atoms.length];cases.push({t,d:(rnd()%5==0)?'||':','});}
const run=c=>new Promise(res=>{const rows=[];const p=parse({delimiter:c.d});let done=false;
 p.on('data',r=>rows.push(r)); p.on('error',e=>{if(!done){done=true;res({rows,err:e.code});}}); p.on('end',()=>{if(!done){done=true;res({rows,err:null});}});
 p.write(c.t); p.end();});
(async()=>{for(const c of cases){const r=await run(c); process.stdout.write(JSON.stringify({text:c.t,delimiter:c.d,rows:r.rows,error:r.err})+'\n');}})();
