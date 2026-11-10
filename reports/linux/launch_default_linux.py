#!/usr/bin/env python3
from pathlib import Path
import subprocess
root=Path(__file__).resolve().parents[2]
mounts=[
 ('bind',root/'work/linux-env/usr','/seed','readonly'),
 ('bind',root/'work/linux-env/etc','/seed-etc','readonly'),
 ('bind',root/'work/linux-env/var','/seed-var','readonly'),
 ('bind',root/'outputs/jfrconv-rs','/project','readonly'),
 ('bind',root/'work/upstream-linux','/work',''),
 ('bind',Path('/Users/v.matskevich/.cargo/registry'),'/work/cargo/registry','readonly'),
]
command=['docker','run','--rm']
for kind,source,target,extra in mounts:
 value=f'type={kind},src={source},dst={target}'
 if extra:value+=','+extra
 command+=['--mount',value]
for target,size in [('/usr',1610612736),('/var',536870912),('/etc',67108864),('/tmp',536870912)]:
 command+=['--tmpfs',f'{target}:rw,exec,nosuid,nodev,size={size}']
loader='/seed/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1'
bootstrap=f'''{loader} --argv0 cp --library-path /seed/lib/aarch64-linux-gnu /seed/bin/gnucp -a --no-preserve=ownership,mode,xattr /seed/. /usr/ &&
{loader} --argv0 chmod --library-path /seed/lib/aarch64-linux-gnu /seed/bin/gnuchmod -R u+rwx /usr &&
/usr/bin/gnuchmod -R a+rX /usr &&
for file in resolv.conf hosts hostname; do /usr/bin/gnucp /etc/$file /tmp/jfrconv-$file; done &&
/usr/bin/gnucp -a --no-preserve=ownership,mode,xattr /seed-etc/. /etc/ &&
for file in resolv.conf hosts hostname; do /usr/bin/gnucp /tmp/jfrconv-$file /etc/$file; done &&
/usr/bin/gnucp -a --no-preserve=ownership,mode,xattr /seed-var/. /var/ &&
/usr/bin/gnuchmod -R u+rwx /etc /var &&
/usr/bin/gnuchmod -R a+rX /etc /var &&
exec /usr/bin/sh /work/run-default-security.sh'''
command+=['--entrypoint',loader,'eclipse-temurin:21-jdk','--argv0','sh','--library-path','/seed/lib/aarch64-linux-gnu','/seed/bin/sh','-c',bootstrap]
raise SystemExit(subprocess.run(command).returncode)
