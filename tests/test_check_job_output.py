"""Silent managed checks communicate completion without another wait command."""
import pytest

from trellis import check_job


@pytest.mark.parametrize('state,code,current,expected', [
    ('succeeded', 0, True, 'check-job: succeeded'),
    ('failed', 1, True, 'check-job: failed'),
    ('succeeded', 0, False, 'workspace changed after submission'),
    ('timed_out', 1, True, 'check-job: timed_out:'),
])
def test_silent_build_reports_final_outcome(monkeypatch, capsys, state, code, current, expected):
    calls = []
    def rpc(request):
        calls.append(request)
        if request['action'] == 'submit':
            return dict(job_id='job1', state='queued')
        return dict(job_id='job1', state=state, current_inputs_match=current,
                    result=dict(returncode=code, stdout='', stderr=''))
    monkeypatch.setattr(check_job, 'configured', lambda: True)
    monkeypatch.setattr(check_job, 'rpc', rpc)
    monkeypatch.setattr(check_job.time, 'sleep', lambda _: None)
    assert check_job.main(['build', 'N']) == (code if current and state != 'timed_out' else 2)
    output = capsys.readouterr()
    assert output.out == ''
    assert 'waiting. If interrupted:' in output.err
    assert expected in output.err
    assert [c['action'] for c in calls] == ['submit', 'status']
    if not current:
        assert 'check-job: succeeded' not in output.err


def test_metadata_output_remains_unadorned(capsys):
    assert check_job.render(dict(state='succeeded', result=dict(
        returncode=0, stdout='/trusted/lean\n', stderr=''))) == 0
    output = capsys.readouterr()
    assert output.out == '/trusted/lean\n'
    assert output.err == ''
