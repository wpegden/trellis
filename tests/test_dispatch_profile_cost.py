import pytest
from trellis.dispatch.profile_cost import comparisons,TOKEN_MIX


def profile(model,lightweight,provider='codex'):
    return dict(model=model,lightweight=lightweight,provider=provider)


def test_fixed_mix_disjoint_and_configured_models_drive_cost():
    assert sum(TOKEN_MIX.values())==pytest.approx(1)
    p={'economy':profile('gpt-6-luna',True),'general':profile('gpt-6.1-sol',False)}
    c=comparisons(p)
    assert c['comparisons'][0]['heavyweight_to_lightweight_token_cost']==15.42
    p['general']['model']='gpt-6-astra'
    assert comparisons(p)['comparisons'][0]['heavyweight_to_lightweight_token_cost']==100
    p['economy']['model']='gpt-6.1-sol'
    assert comparisons(p)['comparisons'][0]['heavyweight_to_lightweight_token_cost']==6.49


def test_new_table_entries_multiple_profiles_and_unknown_prices():
    p={'fast':profile('future-small',True),'slow':profile('future-big',False),
       'missing':profile('unknown',False),'other-provider':profile('future-small',True,'other')}
    prices={'future-small':dict(input=1,cached_input=.1,output=5),
            'future-big':dict(input=3,cached_input=.3,output=15)}
    c=comparisons(p,prices=prices)
    assert len(c['comparisons'])==1
    assert c['comparisons'][0]['heavyweight_to_lightweight_token_cost']==3
    assert c['unavailable_profiles']==['missing','other-provider']
    assert c['weighted_usd_per_million_tokens']['fast']==pytest.approx(.203)


def test_reasoning_is_counted_once_at_output_rate():
    p={'x':profile('test',True)}
    c=comparisons(p,prices={'test':dict(input=0,cached_input=0,output=10)})
    assert c['weighted_usd_per_million_tokens']['x']==pytest.approx(.10)
