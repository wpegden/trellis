"""Principal selection and complete same-assignment session inventory."""
import json
import os
from pathlib import Path
from types import SimpleNamespace
import pytest
from trellis.adapters import ProviderConfig
from trellis.agents import engineering_context as context
from trellis.agent_wrapper.continuation import record_session, session_binding
from test_paper_engineering import root
from trellis.modeling import engineering as eng, engineering_transport as transport
from trellis.modeling.observations import read_json, write_json


@pytest.mark.parametrize('change', ['none', 'sibling', 'root_alias', 'system_alias', 'file_alias', 'fifo', 'hardlink'])
def test_provider_system_skills_are_ordinary_and_private(tmp_path, change):
    home=tmp_path/'home';root=home/'.codex/skills';skill=root/'.system/synthetic-builtin/SKILL.md'
    root.parent.mkdir(parents=True)
    assert context._system_skills(home) is None
    skill.parent.mkdir(parents=True);skill.write_text('synthetic provider builtin')
    if change=='sibling':(root/'task-context').mkdir()
    elif change in ('root_alias','system_alias'):
        path=root if change=='root_alias' else root/'.system'
        original=path.with_name(path.name+'-original');path.rename(original);path.symlink_to(original,target_is_directory=True)
    elif change=='file_alias':
        skill.unlink();skill.symlink_to(tmp_path/'outside')
    elif change=='fifo':skill.unlink();os.mkfifo(skill)
    elif change=='hardlink':os.link(skill,tmp_path/'outside')
    if change=='none':assert set(context._system_skills(home)['tree'])=={'.system/synthetic-builtin/SKILL.md'}
    else:
        with pytest.raises((ValueError,OSError)):context._system_skills(home)


@pytest.mark.parametrize('change', ['unchanged', 'bytes', 'mode', 'symlink', 'hardlink', 'empty_sibling', 'empty_child'])
def test_final_leaf_preserves_complete_retained_skill_predicate(tmp_path, change):
    from trellis.agents import engineering_exec as leaf
    home=tmp_path/'home';view=tmp_path/'view';view.mkdir()
    skill=home/'.codex/skills/.system/owned/SKILL.md';skill.parent.mkdir(parents=True)
    skill.write_text('Synthetic provider-created same-assignment context')
    snapshot=context._system_skills(home)
    assert snapshot==leaf.system_skills_snapshot(home)
    argv=['/usr/bin/true'];environment=leaf.project_environment({},str(home),'/usr/bin:/bin',str(view))
    binding=dict(schema='engineering-launch-v1',argv=argv,home=str(home),work=str(view),runtime_path='/usr/bin:/bin',
        environment_sha256=leaf.digest(environment),links={},absent=[],system_skills=snapshot,
        files={str(Path(leaf.__file__).resolve()):leaf.file_identity(Path(leaf.__file__).resolve())},
        trees=[dict(path=str(home/'.codex/skills'),ignored=[],files=snapshot['tree'])],directories=snapshot['directories'])
    path=tmp_path/'binding.json';raw=json.dumps(binding).encode();path.write_bytes(raw)
    import hashlib
    digest=hashlib.sha256(raw).hexdigest()
    assert leaf.verify(path,digest,argv,{})==environment
    if change=='bytes':skill.write_text('Changed context')
    elif change=='mode':skill.chmod(skill.stat().st_mode^0o100)
    elif change=='symlink':skill.rename(tmp_path/'original');skill.symlink_to(tmp_path/'original')
    elif change=='hardlink':os.link(skill,tmp_path/'alias')
    elif change=='empty_sibling':(home/'.codex/skills/extra').mkdir()
    elif change=='empty_child':(home/'.codex/skills/.system/extra').mkdir()
    if change=='unchanged':assert leaf.verify(path,digest,argv,{})==environment
    else:
        with pytest.raises((ValueError,OSError)):leaf.verify(path,digest,argv,{})


def test_fresh_context_never_accepts_provider_or_inherited_skills(tmp_path):
    home=tmp_path/'home';codex=home/'.codex';codex.mkdir(parents=True)
    (codex/'config.toml').write_text('project_doc_max_bytes=0\n[features]\n'+''.join(f'{name}={str(value).lower()}\n' for name,value in context.FEATURES.items()))
    context.validate(home)
    (codex/'skills/.system').mkdir(parents=True)
    with pytest.raises(ValueError,match='context exclusions'):context.validate(home)


@pytest.mark.parametrize('change', ['none','missing','duplicate','wrong_name','symlink','fifo','parent'])
def test_session_inventory_requires_unique_ordinary_principal(tmp_path, change):
    home=tmp_path/'home';sessions=home/'.codex/sessions';directory=sessions/'2026/10/28';directory.mkdir(parents=True)
    principal=directory/'rollout-2026-10-28T00-00-00-principal.jsonl';principal.write_text('synthetic principal')
    companion=directory/'rollout-2026-10-28T00-00-01-companion.jsonl';companion.write_text('synthetic companion')
    provider=ProviderConfig(provider='codex',model='synthetic',effort='high')
    single=SimpleNamespace(burst_home=home,session_record=tmp_path/'session.json')
    binding=session_binding(provider,home,'principal');record_session(single.session_record,binding)
    if change=='missing':principal.unlink()
    elif change=='duplicate':
        other=sessions/'other';other.mkdir();(other/principal.name).write_text('duplicate principal')
    elif change=='wrong_name':principal.rename(directory/'other-principal.jsonl')
    elif change in ('symlink','fifo'):
        companion.unlink()
        if change=='symlink':companion.symlink_to(principal)
        else:os.mkfifo(companion)
    elif change=='parent':
        outside=tmp_path/'outside';directory.rename(outside);directory.symlink_to(outside,target_is_directory=True)
    if change=='none':
        tree=context._session_tree(single,binding)
        assert set(tree)=={principal.relative_to(sessions).as_posix(),companion.relative_to(sessions).as_posix()}
    else:
        with pytest.raises((ValueError,RuntimeError,OSError)):context._session_tree(single,binding)


def test_all_legacy_purpose_prompts_with_notes_keep_output_contract(root,tmp_path):
    notes=root/eng.STORE/'implementation-notes.md';notes.write_text('Synthetic subordinate guidance.')
    state=read_json(root/'.trellis/modeling/state.json');state['engineering']['input']['implementation_notes']=eng.sha256(notes)
    corpus=[]
    writable={'source_reading':('reading.json',),'plan':('plan.json',),'implementation':('source','plan.json'),
              'plan_review':(),'code_review':(),'retry_review':(),'performance':('benchmarks/harness','benchmarks/report.md')}
    for purpose,expected in writable.items():
        view=tmp_path/('legacy-'+purpose);delivered=eng.delivery(root,state,dict(purpose=purpose),view)
        prompt=(view/'prompt.md').read_text()
        assert transport.writable(purpose)==expected
        assert 'implementation-notes.md supplies guidance subordinate to the paper.' in prompt
        assert 'Record suggested departures in the plan' not in prompt
        if purpose=='source_reading':
            assert 'unspecified choices and proposed departures in reading.json' in prompt and 'plan.json' not in prompt
        if purpose in {'source_reading','plan','implementation'}:
            assert 'Return a JSON object containing only summary, a string describing' in prompt
        if purpose in {'plan_review','code_review'}:assert 'accept|revise|review' in prompt and 'accept|revise|reject' not in prompt
        corpus.append(dict(purpose=purpose,prompt=prompt,inventory=delivered['files'],
          base_instructions=(Path(context.__file__).parent/'codex_base_instructions/engineering.md').read_text()))
    (tmp_path/'legacy-prompt-corpus.json').write_text(json.dumps(corpus,indent=2))
