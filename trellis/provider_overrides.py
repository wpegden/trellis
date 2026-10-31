"""Shared provider binding overrides for math and PV launchers."""
from urllib.parse import urlsplit

PROVIDERS = ('codex', 'claude', 'gemini', 'antigravity', 'deepseek', 'kimi')
API_DEFAULTS = {'deepseek': ('deepseek-v4-pro', 'max'), 'kimi': ('kimi-k3', 'max')}


def validate_endpoint(value):
    url = urlsplit(value)
    if url.scheme != 'https' or not url.hostname or url.username or url.password or url.query or url.fragment:
        raise ValueError('API base URL must be an HTTPS URL without credentials, query, or fragment')
    return value.rstrip('/')


def apply_binding(binding, selected, config, *, endpoints=None):
    provider = selected.get('provider')
    switched = provider and provider != binding.get('provider')
    if provider:
        if provider not in PROVIDERS:
            raise ValueError('Unknown agent provider: ' + provider)
        if provider != binding.get('provider'):
            binding.pop('extra_args', None)
            binding.pop('fallback_models', None)
            if provider in API_DEFAULTS:
                model, effort = API_DEFAULTS[provider]
                binding['model'] = model
                binding['reasoning_effort' if 'reasoning_effort' in binding else 'effort'] = effort
        binding['provider'] = provider
    if selected.get('model'):
        binding['model'] = selected['model']
    if selected.get('effort'):
        binding['reasoning_effort' if 'reasoning_effort' in binding else 'effort'] = selected['effort']
    if switched and 'label' in binding:
        binding['label'] = binding['model']
    if selected.get('base_url'):
        provider = binding['provider']
        if provider not in API_DEFAULTS:
            raise ValueError('API base URL is only supported for DeepSeek and Kimi')
        endpoint = validate_endpoint(selected['base_url'])
        if endpoints is not None:
            if provider in endpoints and endpoints[provider] != endpoint:
                raise ValueError('Conflicting base URLs for provider ' + provider)
            endpoints[provider] = endpoint
        config.setdefault('api_providers', {})[provider] = {'base_url': endpoint}
