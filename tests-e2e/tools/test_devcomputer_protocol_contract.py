import asyncio
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest

from devcomputer_protocol_contract import Contract, container_witness, positive_frame, successful_terminal


class EventContracts(unittest.TestCase):
    def test_container_witness_detects_same_cid_stop_restart_and_mount_change(self):
        row={'Id':'a'*64,'Name':'/owned','Image':'sha256:old','Config':{'Labels':{'owner':'test'}},
             'State':{'Running':True,'StartedAt':'original'},'RestartCount':0,'Mounts':[]}
        expected=container_witness(row)
        for key,value in (('Running',False),('StartedAt','restarted')):
            changed=json.loads(json.dumps(row));changed['State'][key]=value
            self.assertNotEqual(container_witness(changed),expected)
        changed=json.loads(json.dumps(row));changed['RestartCount']=1
        self.assertNotEqual(container_witness(changed),expected)
        changed=json.loads(json.dumps(row));changed['Mounts']=[{'Type':'bind','Source':'/other'}]
        self.assertNotEqual(container_witness(changed),expected)

    def frame(self, sid='opaque/real', rid='original', seq='7', event='end_turn'):
        return SimpleNamespace(event=event, id=seq, data=json.dumps({
            'sessionId':sid,'messageType':'sessionPromptEnd','subType':event,
            'data':{'request_id':rid,'reason':'EndTurn'}}))

    def test_terminal_needs_original_sid_rid_and_positive_sequence(self):
        frame=positive_frame(self.frame(),'opaque/real')
        self.assertTrue(successful_terminal(frame,'opaque/real','original'))
        self.assertFalse(successful_terminal(frame,'opaque/real','other-request'))
        self.assertFalse(successful_terminal(frame,'other-session','original'))
        for value in ('0',None,''):
            with self.subTest(value=value),self.assertRaises(AssertionError):
                positive_frame(self.frame(seq=value),'opaque/real')

    def test_unknown_non_json_business_frame_is_not_ignored(self):
        with self.assertRaises(json.JSONDecodeError):
            positive_frame(SimpleNamespace(event='end_turn',id='1',data='not JSON'),'opaque')

    def test_actual_decoder_comment_and_inherited_id_are_not_task_evidence(self):
        import httpx
        from httpx_sse import EventSource
        response=httpx.Response(200,headers={'content-type':'text/event-stream'},content=(
            b'id: 2\n: keep-alive\n\ndata:\n\n'
            b'event: ping\ndata: {"sessionId":"opaque","messageType":"heartbeat"}\n\n'))
        events=list(EventSource(response).iter_sse())
        self.assertEqual(len(events),3)
        self.assertEqual([e.id for e in events],['2','2','2'])
        self.assertEqual([positive_frame(e,'opaque')for e in events],[None,None,None])

    def test_cleanup_keeps_diagnostic_failure_but_removes_only_captured_runner(self):
        with tempfile.TemporaryDirectory()as directory:
            case=object.__new__(Contract);case.cid='a'*64;case.root=Path(directory)
            case.captured={'image':'sha256:controlled'}
            case.report={'success':True,'checks':[]};calls=[]
            case.fence=lambda **kw:calls.append(('fence',kw))
            def journal():raise OSError('controlled observation unavailable')
            case.journal=journal;case.save=lambda *args:None
            case.docker=lambda *args,**kw:calls.append(args)or(''if args[0]=='ps'else'logs')
            case.check=lambda name,ok,detail=None:self.assertTrue(ok)
            asyncio.run(case.cleanup())
            self.assertFalse(case.report['success'])
            self.assertIn('journal_capture_error',case.report)
            self.assertIn(('rm','-f','a'*64),calls)
            self.assertFalse(any('-v'in row for row in calls))


class ControlledFixtureContracts(unittest.TestCase):
    def exercise(self, opt_in):
        fixture=Path(__file__).resolve().parents[2]/'crates/agent_runner/tests/fixtures/controlled_acp.py'
        # tools -> tests-e2e -> repository
        self.assertTrue(fixture.is_file())
        with tempfile.TemporaryDirectory()as directory:
            root=Path(directory);source=root/'source.txt';source.write_text('changed-real-file')
            env=dict(os.environ,ACP_FIXTURE_DIR=str(root),AGENT_PROJECT_ID='offline-project')
            for key in ('ACP_FIXTURE_HTTP_CONTRACT','ACP_FIXTURE_SOURCE_FILE','ACP_FIXTURE_SESSION_PREFIX'):
                env.pop(key,None)
            if opt_in:
                env.update(ACP_FIXTURE_HTTP_CONTRACT='1',ACP_FIXTURE_SOURCE_FILE=str(source),
                           ACP_FIXTURE_SESSION_PREFIX='unique-offline-')
            child=subprocess.Popen([sys.executable,str(fixture)],stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,env=env)
            output=queue.Queue()
            def read():
                for line in child.stdout:output.put(json.loads(line))
            reader=threading.Thread(target=read,daemon=True);reader.start()
            def send(value):child.stdin.write(json.dumps(value)+'\n');child.stdin.flush()
            try:
                send({'jsonrpc':'2.0','id':1,'method':'initialize'})
                self.assertEqual(output.get(timeout=5)['id'],1)
                send({'jsonrpc':'2.0','id':2,'method':'session/new','params':{'cwd':str(root)}})
                sid=output.get(timeout=5)['result']['sessionId']
                self.assertTrue(sid.startswith('unique-offline-'if opt_in else'fixture-'))
                if opt_in:
                    send({'jsonrpc':'2.0','id':4,'method':'session/load',
                          'params':{'sessionId':'unknown-project-history'}})
                    self.assertEqual(output.get(timeout=5)['error']['code'],-32603)
                    send({'jsonrpc':'2.0','id':5,'method':'session/load','params':{'sessionId':sid}})
                    self.assertEqual(output.get(timeout=5)['result']['sessionId'],sid)
                    send({'jsonrpc':'2.0','id':6,'method':'session/prompt','params':{
                        'sessionId':'foreign-live-session','prompt':[{'type':'text','text':'must not execute'}],
                        '_meta':{'request_id':'wrong-session'}}})
                    self.assertEqual(output.get(timeout=5)['error']['code'],-32602)
                send({'jsonrpc':'2.0','id':3,'method':'session/prompt','params':{
                    'sessionId':sid,'prompt':[{'type':'text','text':'read once'}],
                    '_meta':{'request_id':'original-offline'}}})
                if opt_in:
                    deadline=time.monotonic()+5
                    while True:
                        rows=[json.loads(x)for x in (root/'protocol.jsonl').read_text().splitlines()]
                        if any(r['event']=='prompt'for r in rows):break
                        self.assertLess(time.monotonic(),deadline);time.sleep(.01)
                    self.assertTrue(output.empty(),'no update or terminal before original release')
                    (root/'original-offline.subscriber-probe').write_text('live-original-witness')
                    witness=output.get(timeout=5)
                    self.assertEqual(witness['params']['update']['availableCommands'][0]['name'],
                                     'live-original-witness')
                    (root/'original-offline.release').write_text('release')
                    update=output.get(timeout=5)
                    while update.get('params',{}).get('update',{}).get('sessionUpdate')=='available_commands_update':
                        update=output.get(timeout=5)
                    self.assertEqual(update['params']['update']['content']['text'],'changed-real-file')
                self.assertEqual(output.get(timeout=5)['result']['stopReason'],'end_turn')
                child.stdin.close();self.assertEqual(child.wait(timeout=5),0)
                reader.join(timeout=2)
                rows=[json.loads(x)for x in (root/'protocol.jsonl').read_text().splitlines()]
                self.assertEqual(sum(r['event']=='prompt'for r in rows),1)
                if opt_in:
                    rejected=next(row for row in rows if row['event']=='prompt_wrong_session')
                    self.assertEqual(rejected['received_sid'],'foreign-live-session')
                    accepted=next(row for row in rows if row['event']=='prompt')
                    self.assertEqual(accepted['received_sid'],sid)
            finally:
                if child.poll()is None:child.kill();child.wait(timeout=5)
                child.stdout.close();child.stderr.close()

    def test_default_fixture_keeps_original_immediate_response_behavior(self):
        self.exercise(False)

    def test_opt_in_requires_original_release_and_reads_actual_source(self):
        self.exercise(True)


if __name__=='__main__':unittest.main()


class InvalidSidReapOrderTests(unittest.TestCase):
    def records(self):
        return [
            'INFO agent_abstraction::launcher::lifecycle::spawn: [ProcessReaper] exited project_id=owned-c, pid=123, pgid=123, exit_code=None, signal=Some(9)',
            'INFO chat{trace_id=original-trace}: agent_runner::grpc::chat: [gRPC] Chat completed: success=false',
        ]

    def test_forced_kill_before_rpc_is_valid_even_when_term_barrier_was_not_released(self):
        from devcomputer_protocol_contract import invalid_sid_exit_order
        result=invalid_sid_exit_order('\n'.join(self.records()),'owned-c',123,'original-trace')
        self.assertLess(result['reap_line'],result['completion_line'])

    def test_premature_reply_missing_reap_and_foreign_pid_or_trace_are_rejected(self):
        from devcomputer_protocol_contract import invalid_sid_exit_order
        good=self.records()
        for lines, pid, trace in [(list(reversed(good)),123,'original-trace'),
                                  (good[1:],123,'original-trace'),
                                  (good,124,'original-trace'),
                                  (good,123,'other-trace')]:
            with self.subTest(lines=lines,pid=pid,trace=trace),self.assertRaises(AssertionError):
                invalid_sid_exit_order('\n'.join(lines),'owned-c',pid,trace)
