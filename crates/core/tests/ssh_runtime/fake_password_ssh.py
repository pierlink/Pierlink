#!/usr/bin/python3
"""本地 SSH 替身通过真实 Unix IPC 请求密码；不写入或打印任何密码。"""
import json
import os
import socket
import sys
import time

with open(os.environ['PIERLINK_TEST_SSH_LAUNCHES'], 'a', encoding='ascii') as launches:
    launches.write('1\n')

with socket.socket(socket.AF_UNIX) as channel:
    channel.settimeout(5)
    channel.connect(os.environ['PIERLINK_ASKPASS_PIPE'])
    message = {'version': 1, 'nonce': os.environ['PIERLINK_ASKPASS_NONCE'],
               'prompt': "user@host's password: ", 'kind': 'password'}
    channel.sendall(json.dumps(message).encode('utf-8') + b'\n')
    with channel.makefile('rb') as incoming:
        response = json.loads(incoming.readline(4096))
if response.get('answer') != '测试密码':
    sys.stderr.write('Permission denied (password).\n')
    sys.exit(255)
if '-N' in sys.argv:
    sys.stderr.write('Authenticated to host\n')
    sys.stderr.flush()
    while True:
        time.sleep(1)
# 真正执行 core 写入的采样/带定界符脚本，子进程继承同一进程组，结束时由 core 回收。
os.execv('/bin/sh', ['/bin/sh', '-s'])
