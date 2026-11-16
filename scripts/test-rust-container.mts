import http from 'node:http';
import {execFileSync} from 'node:child_process';
import assert from 'node:assert/strict';
const name='cbgpu-materials-rust-contract-'+process.pid;
const server=http.createServer((req,res)=>{res.setHeader('content-type','application/json');if(req.url?.startsWith('/auth/v1/user')){res.writeHead(401);res.end('{}');return;}res.end('[]');});
await new Promise<void>(resolve=>server.listen(18424,'127.0.0.1',resolve));
try {
 execFileSync('docker',['run','-d','--name',name,'--network','host','--read-only','--cap-drop=ALL','--security-opt','no-new-privileges','--tmpfs','/data:rw,noexec,nosuid,size=16m,uid=65532,gid=65532','-e','CBGPU_MATERIALS_LISTEN=127.0.0.1:18423','-e','CBGPU_SUPABASE_URL=http://127.0.0.1:18424','-e','CBGPU_SUPABASE_ANON_KEY=fixture-public-key',process.env.CBGPU_MATERIALS_TEST_IMAGE || 'cbgpu/materials-rust:candidate'],{stdio:'pipe'});
 let ready=false;for(let i=0;i<50;i++){try{const response=await fetch('http://127.0.0.1:18423/health');if(response.ok){ready=true;break;}}catch{}await new Promise(r=>setTimeout(r,100));}assert(ready,'Rust container must become ready');
 // Awaited subprocess keeps the loopback metadata fixture serving readiness.
 const {spawn}=await import('node:child_process');const child=spawn('docker',['exec',name,'cbgpu-materials','healthcheck'],{stdio:'pipe'});assert.equal(await new Promise(r=>child.on('exit',r)),0);
 assert.equal((await fetch('http://127.0.0.1:18423/materials/quota',{headers:{authorization:'Bearer invalid'}})).status,401);
 assert.equal((await fetch('http://127.0.0.1:18423/materials/00000000-0000-4000-8000-000000000001/content',{method:'PUT',headers:{authorization:'Bearer fixture'},body:'abc'})).status,503);
 console.log('Non-root, read-only Rust container: readiness, healthcheck, auth denial and disabled writes passed.');
}finally {execFileSync('docker',['rm','-f',name],{stdio:'pipe'});server.close();}
