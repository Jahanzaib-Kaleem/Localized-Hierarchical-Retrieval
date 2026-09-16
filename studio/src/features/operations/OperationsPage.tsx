import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { api } from '../../api/client'
import { formatBytes } from '../../components/format'
import { ActionResult, Notice, PageHeader, Panel } from '../../components/ui'

export function OperationsPage() {
  const queryClient = useQueryClient()
  const [retain, setRetain] = useState(2)
  const [result, setResult] = useState<unknown>()
  const finish = async (value: unknown) => { setResult(value); await queryClient.invalidateQueries({ queryKey: ['stats'] }); await queryClient.invalidateQueries({ queryKey: ['generations'] }) }
  const compact = useMutation({ mutationFn: api.compact, onSuccess: finish })
  const vacuum = useMutation({ mutationFn: () => api.vacuum(retain), onSuccess: finish })
  const recover = useMutation({ mutationFn: api.recover, onSuccess: finish })
  const activeError = compact.error ?? vacuum.error ?? recover.error
  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Administrative control" title="Operations" description="Run bounded maintenance transactions deliberately. No admin operation is retried automatically by Studio." />
    {activeError ? <Notice title="Operation failed">{activeError.message}</Notice> : null}
    <div className="operation-grid">
      <Panel title="Compaction" eyebrow="Base + deltas"><div className="operation-card"><p>Stream visible logical rows into a fresh compact generation, rebuild exact structures, then publish atomically.</p><button className="button" disabled={compact.isPending} onClick={() => window.confirm('Start compaction?') && compact.mutate()}>{compact.isPending ? 'Compacting…' : 'Compact dataset'}</button></div></Panel>
      <Panel title="Vacuum" eyebrow="Lease-aware retention"><div className="operation-card"><p>Remove old generations that are outside retention and not protected by active reader leases.</p><label className="field"><span>Retain newest</span><input type="number" min={1} max={100} value={retain} onChange={(event) => setRetain(Math.max(1, Number(event.target.value) || 1))} /></label><button className="button" disabled={vacuum.isPending} onClick={() => window.confirm(`Vacuum generations while retaining ${retain}?`) && vacuum.mutate()}>{vacuum.isPending ? 'Vacuuming…' : 'Vacuum catalog'}</button></div></Panel>
      <Panel title="Recovery" eyebrow="Verification-based"><div className="operation-card"><p>Repoint CURRENT to the newest fully verified published generation and clean abandoned staging work.</p><button className="button" disabled={recover.isPending} onClick={() => window.confirm('Run catalog recovery?') && recover.mutate()}>{recover.isPending ? 'Recovering…' : 'Run recovery'}</button></div></Panel>
    </div>
    {result ? <Panel title="Last operation" eyebrow="Server report"><ActionResult value={result} /></Panel> : null}
    <Panel title="Local-only administration" eyebrow="Filesystem boundary"><dl className="definition-list"><div><dt>Backup</dt><dd><span className="code-chip">lhr backup</span></dd></div><div><dt>Restore</dt><dd><span className="code-chip">lhr restore</span></dd></div><div><dt>Bulk ingest</dt><dd><span className="code-chip">lhr import external</span></dd></div><div><dt>Reason</dt><dd>no arbitrary remote filesystem path capability</dd></div></dl></Panel>
  </div>
}
