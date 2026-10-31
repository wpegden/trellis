"""Advisory profile comparison using fixed workload weights and listed prices.

Rates: https://developers.openai.com/api/docs/pricing (2026-10-10), Standard,
short-context USD/MTok. Reasoning is part of billed output, counted once here.
Update the per-model table when prices/models change; no runtime usage scan.
"""

# One-time calibration includes 96 retained Luna and 38 completed Sol turns.
# Combined disjoint shares were 5.79% uncached / 93.57% cached / .385% visible
# output / .250% reasoning. Sol's output share was higher than Luna's. Use
# rounded 6/93/.5/.5 weights, giving output a 1% share versus observed .635%.
# These fixed weights are a comparison convention, never live usage tracking.
TOKEN_MIX = {'input': .06, 'cached_input': .93, 'output': .005, 'reasoning': .005}
PRICE_DATE = '2026-10-10'
PRICES = {
    'gpt-6-luna': {'input': .10, 'cached_input': .01, 'output': .50},
    'gpt-6-sol': {'input': 2.00, 'cached_input': .20, 'output': 10.00},
    'gpt-6.1-sol': {'input': 2.00, 'cached_input': .10, 'output': 10.00},
    'gpt-6-astra': {'input': 10.00, 'cached_input': 1.00, 'output': 50.00},
    'gpt-5.6-sol': {'input': 4.00, 'cached_input': .40, 'output': 20.00},
}


def comparisons(profiles, *, prices=None):
    prices = PRICES if prices is None else prices
    costs = {}
    for name, profile in profiles.items():
        rate = prices.get(profile.get('model')) if profile.get('provider') == 'codex' else None
        if rate is not None:
            costs[name] = (TOKEN_MIX['input'] * rate['input']
                + TOKEN_MIX['cached_input'] * rate['cached_input']
                + (TOKEN_MIX['output'] + TOKEN_MIX['reasoning']) * rate['output'])
    pairs = []
    for light, lp in profiles.items():
        if lp.get('lightweight') is not True or light not in costs:
            continue
        for heavy, hp in profiles.items():
            if hp.get('lightweight') is not False or heavy not in costs:
                continue
            pairs.append(dict(lightweight_profile=light, heavyweight_profile=heavy,
                lightweight_model=lp['model'], heavyweight_model=hp['model'],
                heavyweight_to_lightweight_token_cost=round(costs[heavy] / costs[light], 2)))
    return dict(basis='Listed Standard short-context API token prices; fixed representative mix',
        prices_checked=PRICE_DATE, price_source='https://developers.openai.com/api/docs/pricing',
        token_mix=TOKEN_MIX, weighted_usd_per_million_tokens={k:round(v,6) for k,v in costs.items()},
        comparisons=pairs, unavailable_profiles=[k for k in profiles if k not in costs],
        interpretation='A ratio of R means the heavyweight profile costs R times as much for the same fixed token mix. '
            'Frequent lower-cost closure attempts on promising nodes can be economical with occasional successes. '
            'Evaluate compiler time, slot occupancy and task fit alongside token cost. Actual billing depends on service tier, context size and usage.')
