"""End-to-end synthetic download/extract/FTP test; never contacts a real Xbox/store."""
import argparse
import io
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import threading
import time
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.request import Request, urlopen
from urllib.error import HTTPError

def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0)); return sock.getsockname()[1]

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('binary',type=Path)
    parser.add_argument('rclone',type=Path)
    args=parser.parse_args()
    payload=bytearray(52)
    payload[:4]=b'XEX2'; payload[20:24]=(1).to_bytes(4,'big')
    payload[24:28]=(0x40006).to_bytes(4,'big'); payload[28:32]=(32).to_bytes(4,'big')
    payload[44:48]=(0xF00D0042).to_bytes(4,'big'); payload[50:52]=b'\x01\x01'
    archive=io.BytesIO()
    with zipfile.ZipFile(archive,'w') as zipped:
        zipped.writestr('Synthetic/default.xex',payload)
        zipped.writestr('Synthetic/data.bin',b'fixture-content'*(2*1024*1024))
    class Fixture(BaseHTTPRequestHandler):
        def do_HEAD(self): self.respond(True)
        def do_GET(self): self.respond(False)
        def respond(self,head):
            if self.path.endswith('.zip'):
                time.sleep(1)
                body=archive.getvalue() if 'good.zip' in self.path else b'not an archive'
                mime='application/zip'
            else:
                bad=self.path.split('?')[0].endswith('/43')
                name='bad.zip' if bad else 'good.zip'
                body=json.dumps({'id':43 if bad else 42,'link':base+'/homebrew',
                    'title':{'rendered':'Broken Fixture' if bad else 'Synthetic Fixture'},
                    'content':{'rendered':f'Game : <a href="{base}/{name}">Direct</a>'}}).encode()
                mime='application/json'
            self.send_response(200); self.send_header('Content-Type',mime)
            self.send_header('Content-Length',str(len(body))); self.end_headers()
            if not head:self.wfile.write(body)
        def log_message(self,*args): pass
    http=ThreadingHTTPServer(('127.0.0.1',0),Fixture)
    base=f'http://127.0.0.1:{http.server_port}'
    threading.Thread(target=http.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='iso2god-delivery-') as temporary:
        root=Path(temporary); config=root/'config'/'iso2god'; config.mkdir(parents=True)
        ftp_root=root/'ftp'; (ftp_root/'Hdd1').mkdir(parents=True)
        ftp_port,api_port,background_port=port(),port(),port(); token=secrets.token_hex(32)
        console=root/'console.json'
        console.write_text(json.dumps(dict(host='127.0.0.1',port=ftp_port,background_port=background_port,username='fixture',password='fixture')))
        (config/'app-settings.txt').write_text(f'store_base_url={base}\n')
        (config/'store-catalog.json').write_text(json.dumps(dict(fetched_at=int(time.time()),entries=[
            dict(id=42,title='Synthetic Fixture',page_url=base+'/good'),dict(id=43,title='Broken Fixture',page_url=base+'/bad') ])))
        env=dict(os.environ,XDG_CONFIG_HOME=str(root/'config'),APPDATA=str(root/'config'),ISO2GOD_WORKER_TOKEN=token,
                 ISO2GOD_WORKER_BIND=f'127.0.0.1:{api_port}',ISO2GOD_WORKER_CONSOLE=str(console),RCLONE_BWLIMIT='1M')
        flags=getattr(subprocess,'CREATE_NO_WINDOW',0)
        log=(root/'test.log').open('w')
        ftp=subprocess.Popen([str(args.rclone.resolve()),'serve','ftp',str(ftp_root),'--addr',f'127.0.0.1:{ftp_port}',
            '--user','fixture','--pass','fixture'],stdout=log,stderr=log,creationflags=flags)
        background=None
        worker=subprocess.Popen([str(args.binary.resolve()),'--store-worker-service'],env=env,stdout=log,stderr=log,creationflags=flags)
        def request(action,**fields):
            data=json.dumps(dict(action=action,**fields)).encode()
            req=Request(f'http://127.0.0.1:{api_port}/v1/console',data=data,headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
            try:
                with urlopen(req,timeout=30) as response:return response.status,response.read().decode()
            except HTTPError as error:return error.code,error.read().decode()
        try:
            for _ in range(100):
                try:
                    if request('jobs')[0]==200:break
                except OSError:time.sleep(.1)
            else:raise RuntimeError('Worker did not start')
            assert request('search',value='Fixture')[0]==200
            assert request('activity')==(200,'Idle\n')
            assert request('notifications')==(200,'\n')
            assert request('queue',value='42',index=0,drive='../../other')[0]==400
            assert request('credentials',host='192.0.2.1')[0]==400
            status,body=request('connection')
            assert status==200 and 'fixture' not in body,(status,body)
            assert request('drives')[0]==200
            saved_console=json.loads(console.read_text())
            console.write_text(json.dumps(dict(saved_console,password='wrong-password')))
            status,body=request('connection')
            assert status==400 and 'FTP_LOGIN_REQUIRED:' in body,(status,body)
            console.write_text(json.dumps(dict(saved_console,password='')))
            status,body=request('connection')
            assert status==400 and 'FTP_LOGIN_REQUIRED:' in body,(status,body)
            status,body=request('drives')
            assert status==400 and 'FTP_LOGIN_REQUIRED:' in body,(status,body)
            status,body=request('credentials',value='fixture',password='wrong-password')
            assert status==400 and 'FTP_LOGIN_REQUIRED:' in body,(status,body)
            assert json.loads(console.read_text())['password']==''
            status,body=request('credentials',value='fixture',password='fixture')
            assert status==200,(status,body)
            assert 'fixture' not in body
            assert request('drives')[0]==200
            # The Xbox store is open: dashboard FTP is down, background FTP is up.
            ftp.terminate(); ftp.wait(timeout=10)
            background=subprocess.Popen([str(args.rclone.resolve()),'serve','ftp',str(ftp_root),'--addr',f'127.0.0.1:{background_port}',
                '--user','fixture','--pass','fixture'],stdout=log,stderr=log,creationflags=flags)
            for listing,expected in [('42','complete'),('43','failed')]:
                status,body=request('options',value=listing); assert status==200,(status,body)
                status,body=request('queue',value=listing,index=0,drive='Hdd1'); assert status==200,(status,body)
                assert 'exit\t' not in request('activity')[1], 'Exit offered during preparation'
                assert request('leave')[0]==400, 'Early Exit was accepted'
                deadline=time.monotonic()+240
                phase=0
                while time.monotonic()<deadline:
                    queue=json.loads((config/'worker/queue.json').read_text())
                    job=next(job for job in queue if job['listing']['id']==listing)
                    progress_path=config/'worker'/job['id']/'progress.json'
                    try:
                        progress=json.loads(progress_path.read_text()) if progress_path.exists() else {}
                    except (PermissionError, FileNotFoundError, json.JSONDecodeError):
                        time.sleep(.1); continue
                    stage=progress.get('stage','')
                    if listing=='42':
                        if phase==0 and stage=='Uploading via background FTP':
                            assert not (progress_path.parent/'dashboard-release.json').exists()
                            assert 'exit\t' not in request('activity')[1]
                            assert 'store\t' not in request('activity')[1]
                            assert request('leave')[0]==400
                            # Bringing dashboard FTP online must not interrupt the chosen service.
                            ftp=subprocess.Popen([str(args.rclone.resolve()),'serve','ftp',str(ftp_root),'--addr',f'127.0.0.1:{ftp_port}',
                                '--user','fixture','--pass','fixture'],stdout=log,stderr=log,creationflags=flags)
                            phase=1
                        elif phase==1:
                            assert stage!='Uploading via dashboard FTP', 'Unexpected switch to dashboard'
                            if progress.get('percent',0)>10:
                                background.terminate(); background.wait(timeout=10); phase=2
                        elif phase==2 and stage=='Transfer paused':
                            background=subprocess.Popen([str(args.rclone.resolve()),'serve','ftp',str(ftp_root),'--addr',f'127.0.0.1:{background_port}',
                                '--user','fixture','--pass','fixture'],stdout=log,stderr=log,creationflags=flags)
                            phase=3
                        elif phase==3 and stage=='Uploading via background FTP':
                            phase=4
                        if job['status']=='running':
                            activity=request('activity')[1]
                            assert 'exit\t' not in activity and 'store\t' not in activity
                    if job['status'] in ('complete','failed'):break
                    time.sleep(.25)
                assert job['status']==expected,(job,[(p.name,p.read_text(errors='replace')) for p in (config/'worker').rglob('failure.txt')])
                if listing=='42': assert phase==4, f'FTP transition test stopped at phase {phase}'
                assert 'store\tReturn to main menu' in request('activity')[1]
                assert request('leave')[0]==200
                code,notice=request('notifications')
                assert code==200 and notice.startswith(job['id']+'\n'),notice
                if expected=='complete': assert 'Rescan your Xbox library' in notice,notice
                else: assert 'Could not finish' in notice and 'Ready.' not in notice,notice
                assert request('ack_notification',value=job['id'])[0]==200
                assert request('notifications')==(200,'\n')
                code,activity=request('activity')
                assert code==200 and ('retry:' in activity)==(expected=='failed'),activity
            delivered=ftp_root/'Hdd1/Content/0000000000000000/F00D0042'
            assert (delivered/'default.xex').read_bytes()==payload
            assert (delivered/'data.bin').read_bytes()==b'fixture-content'*(2*1024*1024)
            worker.terminate(); worker.wait(timeout=10)
            worker=subprocess.Popen([str(args.binary.resolve()),'--store-worker-service'],env=env,stdout=log,stderr=log,creationflags=flags)
            for _ in range(100):
                try:
                    code,body=request('jobs')
                    if code==200:break
                except OSError:time.sleep(.1)
            assert 'complete' in body and 'failed' in body
            assert request('connection')[0]==200
            assert request('notifications')==(200,'\n')
            print('PASS: synthetic install, locked progress until completion, same-service reconnect, verified bytes, failure/retry and persistence')
        finally:
            if os.name == 'nt' and worker.poll() is None:
                subprocess.run(['taskkill','/PID',str(worker.pid),'/T','/F'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            # Direct termination still works if taskkill is unavailable/restricted.
            if worker.poll() is None:
                worker.terminate()
            worker.wait(timeout=10)
            ftp.terminate(); ftp.wait(timeout=10)
            if background is not None: background.terminate(); background.wait(timeout=10)
            log.close()
    http.shutdown()

if __name__=='__main__':main()
