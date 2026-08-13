const modules = import.meta.glob<string>('../assets/logos/*', {
  eager: true,
  query: '?url',
  import: 'default',
})

const LOGOS: Record<string, string> = {}
for (const [path, mod] of Object.entries(modules)) {
  const key = path.split('/').pop()!.replace(/\.(png|svg)$/, '')
  LOGOS[key] = mod
}

const BRAND_PREFIXES: Array<[string, string]> = [
  ['deepseek', 'deepseek'],
  ['glm', 'glm'],
  ['grok', 'grok'],
  ['qwen', 'qwen'],
  ['minimax', 'minimax'],
  ['mimo', 'mimo'],
  ['hy3', 'hy3'],
  ['hunyuan', 'hy3'],
  ['claude', 'anthropic'],
  ['opus', 'anthropic'],
  ['sonnet', 'anthropic'],
  ['haiku', 'anthropic'],
]

export function modelLogoSrc(modelId: string): string | null {
  const segment = modelId.split(/[/:]/).pop()!.toLowerCase()
  for (const [prefix, brand] of BRAND_PREFIXES) {
    if (segment.startsWith(prefix)) return LOGOS[brand] ?? null
  }
  return null
}

export function modelMonogram(modelId: string): string {
  const segment = modelId.split(/[/:]/).pop()?.toLowerCase() ?? ''
  if (segment.startsWith('gpt')) return 'G'
  if (segment.startsWith('kimi') || segment.startsWith('k3')) return 'K'
  return segment.match(/[a-z0-9]/)?.[0]?.toUpperCase() ?? '?'
}
