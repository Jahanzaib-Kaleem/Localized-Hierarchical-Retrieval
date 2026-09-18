import { useEffect, useMemo, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api } from '../../api/client'
import type { CsvImportReport, ImportDatasetSchema, LogicalType, Normalization } from '../../api/types'
import { formatBytes, numberFormat } from '../../components/format'
import { FileIcon, UploadIcon } from '../../components/icons'
import { EmptyState, Notice, PageHeader, Panel, StatusMark } from '../../components/ui'
import { previewCsv, type CsvPreview } from './csv'

const PAGE_SIZE = 50

const logicalTypes: Array<{ value: LogicalType; label: string }> = [
  { value: 'text', label: 'Text' },
  { value: 'unsigned', label: 'Unsigned integer' },
  { value: 'signed', label: 'Signed integer' },
  { value: 'boolean', label: 'Boolean' },
  { value: 'timestamp', label: 'Timestamp' },
]
const normalizations: Array<{ value: Normalization; label: string }> = [
  { value: 'none', label: 'None' },
  { value: 'trim', label: 'Trim' },
  { value: 'lowercase', label: 'Lowercase' },
  { value: 'trim_lowercase', label: 'Trim + lowercase' },
]

function slugify(value: string) {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 64)
}

export function DataPage() {
  const queryClient = useQueryClient()
  const inputRef = useRef<HTMLInputElement>(null)
  const buckets = useQuery({ queryKey: ['buckets'], queryFn: ({ signal }) => api.buckets(signal), staleTime: 10_000 })
  const [selectedBucket, setSelectedBucket] = useState('default')
  const selected = buckets.data?.find((bucket) => bucket.id === selectedBucket)
  const selectedReady = Boolean(selected?.ready)

  const stats = useQuery({
    queryKey: ['stats', selectedBucket],
    queryFn: ({ signal }) => api.stats(signal, selectedBucket),
    enabled: selectedReady,
    staleTime: 30_000,
    retry: false,
  })

  const [pageCursors, setPageCursors] = useState<Array<number | null>>([null])
  const [pageIndex, setPageIndex] = useState(0)
  const cursor = pageCursors[pageIndex] ?? null
  const browse = useQuery({
    queryKey: ['bucket-browse', selectedBucket, cursor],
    queryFn: ({ signal }) => api.query({
      bucket: selectedBucket,
      filters: [],
      limit: PAGE_SIZE,
      after_row_id: cursor,
    }, signal),
    enabled: selectedReady,
    staleTime: 15_000,
    retry: false,
  })
  const [selectedRows, setSelectedRows] = useState<Set<number>>(new Set())
  const [transferDestination, setTransferDestination] = useState('')

  const [file, setFile] = useState<File | null>(null)
  const [preview, setPreview] = useState<CsvPreview | null>(null)
  const [fileError, setFileError] = useState('')
  const [dragging, setDragging] = useState(false)
  const [lastImport, setLastImport] = useState<CsvImportReport | null>(null)

  const [newBucketName, setNewBucketName] = useState('')
  const [newBucketId, setNewBucketId] = useState('')
  const [renameValue, setRenameValue] = useState('')
  const [combineSources, setCombineSources] = useState<Set<string>>(new Set())
  const [combineName, setCombineName] = useState('')
  const [combineId, setCombineId] = useState('')

  useEffect(() => {
    if (buckets.data?.length && !buckets.data.some((bucket) => bucket.id === selectedBucket)) {
      setSelectedBucket(buckets.data[0].id)
    }
  }, [buckets.data, selectedBucket])

  useEffect(() => {
    setPageCursors([null])
    setPageIndex(0)
    setSelectedRows(new Set())
    setTransferDestination('')
    setFile(null)
    setPreview(null)
    setLastImport(null)
  }, [selectedBucket])

  useEffect(() => {
    setRenameValue(selected?.name ?? '')
  }, [selected?.name])

  const destinationBuckets = useMemo(
    () => (buckets.data ?? []).filter((bucket) => bucket.id !== selectedBucket),
    [buckets.data, selectedBucket],
  )

  const invalidateData = async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['buckets'] }),
      queryClient.invalidateQueries({ queryKey: ['stats'] }),
      queryClient.invalidateQueries({ queryKey: ['bucket-browse'] }),
      queryClient.invalidateQueries({ queryKey: ['ready'] }),
      queryClient.invalidateQueries({ queryKey: ['generations'] }),
    ])
  }

  const createBucketMutation = useMutation({
    mutationFn: () => api.createBucket(newBucketId, newBucketName),
    onSuccess: async (bucket) => {
      setNewBucketName('')
      setNewBucketId('')
      await invalidateData()
      setSelectedBucket(bucket.id)
    },
  })

  const renameBucketMutation = useMutation({
    mutationFn: () => api.renameBucket(selectedBucket, renameValue),
    onSuccess: invalidateData,
  })

  const deleteBucketMutation = useMutation({
    mutationFn: () => api.deleteBucket(selectedBucket),
    onSuccess: async () => {
      setSelectedBucket('default')
      await invalidateData()
    },
  })

  const combineMutation = useMutation({
    mutationFn: () => api.combineBuckets([...combineSources], combineId, combineName),
    onSuccess: async (report) => {
      setCombineSources(new Set())
      setCombineName('')
      setCombineId('')
      await invalidateData()
      setSelectedBucket(report.target.id)
    },
  })

  const transferMutation = useMutation({
    mutationFn: (moveRows: boolean) => api.transferRows(
      selectedBucket,
      transferDestination,
      [...selectedRows],
      moveRows,
    ),
    onSuccess: async () => {
      setSelectedRows(new Set())
      setPageCursors([null])
      setPageIndex(0)
      await invalidateData()
    },
  })

  const selectFile = async (next: File) => {
    setFileError('')
    setLastImport(null)
    if (!next.name.toLowerCase().endsWith('.csv')) {
      setFileError('Choose a .csv file.')
      return
    }
    try {
      const nextPreview = await previewCsv(next)
      setFile(next)
      setPreview(nextPreview)
    } catch (error) {
      setFile(null)
      setPreview(null)
      setFileError(error instanceof Error ? error.message : String(error))
    }
  }

  const updateSchema = (updater: (schema: ImportDatasetSchema) => ImportDatasetSchema) => {
    setPreview((current) => current ? { ...current, schema: updater(current.schema) } : current)
  }

  const importData = useMutation({
    mutationFn: async () => {
      if (!file || !preview) throw new Error('Choose a CSV first.')
      return api.importCsv(file, preview.schema, selectedBucket)
    },
    onSuccess: async (report) => {
      setLastImport(report)
      setFile(null)
      setPreview(null)
      setPageCursors([null])
      setPageIndex(0)
      await invalidateData()
    },
  })

  const startImport = () => {
    if (!file || !preview) return
    if (selectedReady && !window.confirm('Importing this CSV will publish it as the new current dataset inside "' + (selected?.name ?? selectedBucket) + '". Previous generations remain available for recovery. Continue?')) return
    importData.mutate()
  }

  const createBucket = () => {
    const id = newBucketId || slugify(newBucketName)
    if (!id || !newBucketName.trim()) return
    if (id !== newBucketId) setNewBucketId(id)
    createBucketMutation.mutate()
  }

  const nextPage = () => {
    const next = browse.data?.next_cursor
    if (next == null) return
    setPageCursors((current) => [...current.slice(0, pageIndex + 1), next])
    setPageIndex((current) => current + 1)
    setSelectedRows(new Set())
  }

  const previousPage = () => {
    if (pageIndex === 0) return
    setPageIndex((current) => Math.max(0, current - 1))
    setSelectedRows(new Set())
  }

  const pageRows = browse.data?.rows ?? []
  const pageRowIds = pageRows.map((row) => row.row_id)
  const allPageSelected = pageRowIds.length > 0 && pageRowIds.every((rowId) => selectedRows.has(rowId))
  const columns = pageRows[0]?.values.map((value) => value.column) ?? stats.data?.column_stats.map((column) => column.name) ?? []

  return <div className="page stack stack--lg">
    <PageHeader
      eyebrow="Database"
      title="Data"
      description="Organize independent datasets into buckets, browse real rows, import files, and move data without leaving Studio."
      action={buckets.data?.length ? <label className="bucket-select">
        <span>Active bucket</span>
        <select value={selectedBucket} onChange={(event) => setSelectedBucket(event.target.value)}>
          {buckets.data.map((bucket) => <option key={bucket.id} value={bucket.id}>{bucket.name + ' · ' + (bucket.ready ? numberFormat.format(bucket.rows) : 'empty')}</option>)}
        </select>
      </label> : undefined}
    />

    {lastImport ? <Notice title="Dataset imported">Published generation {lastImport.generation.id} with {numberFormat.format(lastImport.rows)} rows in {selected?.name ?? selectedBucket}.</Notice> : null}

    <Panel title="Buckets" eyebrow={(buckets.data?.length ?? 0) + ' workspaces'}>
      <div className="bucket-layout">
        <div className="bucket-list" role="list">
          {(buckets.data ?? []).map((bucket) => <button
            className="bucket-card"
            data-active={bucket.id === selectedBucket ? 'true' : 'false'}
            key={bucket.id}
            type="button"
            onClick={() => setSelectedBucket(bucket.id)}
          >
            <div className="bucket-card__heading"><StatusMark active={bucket.ready} /><strong>{bucket.name}</strong>{bucket.is_default ? <span className="code-chip">default</span> : null}</div>
            <span>{bucket.ready ? numberFormat.format(bucket.rows) + ' rows · ' + formatBytes(bucket.total_bytes) : 'Empty · ready for import'}</span>
            <code>{bucket.id}</code>
          </button>)}
          {buckets.isLoading ? <span className="field-hint">Loading buckets…</span> : null}
        </div>

        <div className="bucket-admin stack">
          <div className="bucket-admin__section">
            <span className="eyebrow">Create bucket</span>
            <div className="form-grid">
              <label className="field field--grow"><span>Name</span><input value={newBucketName} onChange={(event) => {
                const value = event.target.value
                setNewBucketName(value)
                if (!newBucketId || newBucketId === slugify(newBucketName)) setNewBucketId(slugify(value))
              }} placeholder="Florida coffee shops" /></label>
              <label className="field field--grow"><span>ID</span><input value={newBucketId} onChange={(event) => setNewBucketId(slugify(event.target.value))} placeholder="florida-coffee-shops" /></label>
              <button className="button button--primary" type="button" disabled={!newBucketName.trim() || !newBucketId || createBucketMutation.isPending} onClick={createBucket}>Create</button>
            </div>
            {createBucketMutation.error ? <Notice title="Could not create bucket">{createBucketMutation.error.message}</Notice> : null}
          </div>

          {selected ? <div className="bucket-admin__section">
            <span className="eyebrow">Selected bucket</span>
            <div className="form-grid">
              <label className="field field--grow"><span>Display name</span><input value={renameValue} onChange={(event) => setRenameValue(event.target.value)} /></label>
              <button className="button button--quiet" type="button" disabled={!renameValue.trim() || renameValue.trim() === selected.name || renameBucketMutation.isPending} onClick={() => renameBucketMutation.mutate()}>Rename</button>
              {!selected.is_default ? <button className="button button--danger" type="button" disabled={deleteBucketMutation.isPending} onClick={() => {
                if (window.confirm('Delete bucket "' + selected.name + '" and all of its generations? This cannot be undone.')) deleteBucketMutation.mutate()
              }}>Delete</button> : null}
            </div>
            {renameBucketMutation.error ? <Notice title="Rename failed">{renameBucketMutation.error.message}</Notice> : null}
            {deleteBucketMutation.error ? <Notice title="Delete failed">{deleteBucketMutation.error.message}</Notice> : null}
          </div> : null}
        </div>
      </div>
    </Panel>

    <Panel
      title={selected ? selected.name : 'Database rows'}
      eyebrow={selectedReady ? numberFormat.format(selected?.rows ?? 0) + ' rows · 50 per page' : 'No dataset yet'}
      action={selectedReady ? <div className="button-group">
        <button className="button button--quiet" type="button" disabled={pageIndex === 0 || browse.isFetching} onClick={previousPage}>Previous</button>
        <span className="page-counter">Page {pageIndex + 1}</span>
        <button className="button button--quiet" type="button" disabled={browse.data?.next_cursor == null || browse.isFetching} onClick={nextPage}>Next</button>
      </div> : undefined}
    >
      {!selectedReady ? <EmptyState>This bucket is empty. Import a CSV below to create its first dataset.</EmptyState> : <>
        {browse.error ? <Notice title="Could not browse rows">{browse.error.message}</Notice> : null}
        <div className="database-toolbar">
          <span>{selectedRows.size ? selectedRows.size + ' selected' : pageRows.length + ' rows on this page'}</span>
          {selectedRows.size ? <div className="button-group">
            <select aria-label="Transfer destination" value={transferDestination} onChange={(event) => setTransferDestination(event.target.value)}>
              <option value="">Choose destination…</option>
              {destinationBuckets.map((bucket) => <option key={bucket.id} value={bucket.id}>{bucket.name}</option>)}
            </select>
            <button className="button button--quiet" type="button" disabled={!transferDestination || transferMutation.isPending} onClick={() => transferMutation.mutate(false)}>Copy</button>
            <button className="button button--quiet" type="button" disabled={!transferDestination || transferMutation.isPending} onClick={() => transferMutation.mutate(true)}>Move</button>
          </div> : null}
        </div>
        {transferMutation.data?.warning ? <Notice title="Move completed with a warning">{transferMutation.data.warning}</Notice> : null}
        {transferMutation.error ? <Notice title="Transfer failed">{transferMutation.error.message}</Notice> : null}
        <div className="table-wrap table-wrap--database">
          <table className="data-table data-table--database">
            <thead><tr>
              <th className="database-select-cell"><input aria-label="Select page" type="checkbox" checked={allPageSelected} onChange={(event) => setSelectedRows(event.target.checked ? new Set(pageRowIds) : new Set())} /></th>
              <th>row_id</th>
              {columns.map((column) => <th key={column}>{column}</th>)}
            </tr></thead>
            <tbody>{pageRows.map((row) => {
              const values = new Map(row.values.map((value) => [value.column, value.value]))
              return <tr key={row.row_id} data-selected={selectedRows.has(row.row_id) ? 'true' : 'false'}>
                <td className="database-select-cell"><input aria-label={'Select row ' + row.row_id} type="checkbox" checked={selectedRows.has(row.row_id)} onChange={(event) => setSelectedRows((current) => {
                  const next = new Set(current)
                  if (event.target.checked) next.add(row.row_id)
                  else next.delete(row.row_id)
                  return next
                })} /></td>
                <td className="mono data-table__primary">{numberFormat.format(row.row_id)}</td>
                {columns.map((column) => <td key={column} title={values.get(column) ?? 'NULL'}>{values.get(column) ?? <span className="null-value">NULL</span>}</td>)}
              </tr>
            })}</tbody>
          </table>
        </div>
        {browse.isFetching ? <p className="field-hint">Loading page…</p> : null}
      </>}
    </Panel>

    <Panel title={selectedReady ? 'Import into ' + (selected?.name ?? selectedBucket) : 'Initialize ' + (selected?.name ?? selectedBucket)} eyebrow="CSV">
      <div className="stack">
        <div
          className="drop-zone"
          data-dragging={dragging ? 'true' : 'false'}
          onDragEnter={(event) => { event.preventDefault(); setDragging(true) }}
          onDragOver={(event) => event.preventDefault()}
          onDragLeave={(event) => { event.preventDefault(); if (event.currentTarget === event.target) setDragging(false) }}
          onDrop={(event) => { event.preventDefault(); setDragging(false); const dropped = event.dataTransfer.files[0]; if (dropped) void selectFile(dropped) }}
          onClick={() => inputRef.current?.click()}
          role="button"
          tabIndex={0}
          onKeyDown={(event) => { if (event.key === 'Enter' || event.key === ' ') inputRef.current?.click() }}
        >
          <input ref={inputRef} className="visually-hidden" type="file" accept=".csv,text/csv" onChange={(event) => { const chosen = event.target.files?.[0]; if (chosen) void selectFile(chosen); event.currentTarget.value = '' }} />
          <div className="drop-zone__icon">{file ? <FileIcon /> : <UploadIcon />}</div>
          <div className="drop-zone__copy">
            <strong>{file ? file.name : 'Drop a CSV here'}</strong>
            <span>{file ? formatBytes(file.size) + ' · click to choose another file' : 'or click to choose a file'}</span>
          </div>
          <span className="button button--quiet">Choose CSV</span>
        </div>
        {fileError ? <Notice title="Could not read CSV">{fileError}</Notice> : null}
        <p className="field-hint">Studio previews only a small sample in the browser. The actual import is streamed into the active bucket and built on the server.</p>
      </div>
    </Panel>

    {preview ? <>
      <Panel title="Review columns" eyebrow={preview.schema.columns.length + ' detected'} action={<button className="button button--primary" disabled={importData.isPending} type="button" onClick={startImport}>{importData.isPending ? 'Importing…' : selectedReady ? 'Publish new dataset' : 'Create dataset'}</button>}>
        <div className="table-wrap schema-editor-wrap"><table className="data-table schema-editor"><thead><tr><th>Column</th><th>Type</th><th>Nullable</th><th>Normalization</th></tr></thead><tbody>
          {preview.schema.columns.map((column, index) => <tr key={column.name + '-' + index}>
            <td className="data-table__primary">{column.name}</td>
            <td><select value={column.logical_type} onChange={(event) => updateSchema((schema) => ({ ...schema, columns: schema.columns.map((item, i) => i === index ? { ...item, logical_type: event.target.value as LogicalType } : item) }))}>{logicalTypes.map((type) => <option value={type.value} key={type.value}>{type.label}</option>)}</select></td>
            <td><label className="toggle-field"><input type="checkbox" checked={column.nullable} onChange={(event) => updateSchema((schema) => ({ ...schema, columns: schema.columns.map((item, i) => i === index ? { ...item, nullable: event.target.checked, null_values: event.target.checked ? [''] : [] } : item) }))} /><span>{column.nullable ? 'Yes' : 'No'}</span></label></td>
            <td><select value={column.normalization} onChange={(event) => updateSchema((schema) => ({ ...schema, columns: schema.columns.map((item, i) => i === index ? { ...item, normalization: event.target.value as Normalization } : item) }))}>{normalizations.map((normalization) => <option value={normalization.value} key={normalization.value}>{normalization.label}</option>)}</select></td>
          </tr>)}
        </tbody></table></div>
        {importData.error ? <Notice title="Import failed">{importData.error.message}</Notice> : null}
      </Panel>

      <Panel title="Preview" eyebrow="First complete rows">
        <div className="table-wrap preview-table-wrap"><table className="data-table"><thead><tr>{preview.headers.map((header) => <th key={header}>{header}</th>)}</tr></thead><tbody>{preview.rows.map((row, rowIndex) => <tr key={rowIndex}>{preview.headers.map((header, columnIndex) => <td key={rowIndex + '-' + header} title={row[columnIndex] ?? ''}>{row[columnIndex] || <span className="null-value">empty</span>}</td>)}</tr>)}</tbody></table></div>
      </Panel>
    </> : null}

    <Panel title="Combine buckets" eyebrow="Build a new bucket">
      <div className="stack">
        <p className="support-copy">Select ready buckets with identical schemas. LHR streams their visible rows into one new bucket without loading the combined dataset into RAM.</p>
        <div className="bucket-source-grid">
          {(buckets.data ?? []).filter((bucket) => bucket.ready).map((bucket) => <label className="bucket-source" key={bucket.id}>
            <input type="checkbox" checked={combineSources.has(bucket.id)} onChange={(event) => setCombineSources((current) => {
              const next = new Set(current)
              if (event.target.checked) next.add(bucket.id)
              else next.delete(bucket.id)
              return next
            })} />
            <span><strong>{bucket.name}</strong><small>{numberFormat.format(bucket.rows)} rows</small></span>
          </label>)}
        </div>
        <div className="form-grid">
          <label className="field field--grow"><span>New bucket name</span><input value={combineName} onChange={(event) => {
            const value = event.target.value
            setCombineName(value)
            if (!combineId || combineId === slugify(combineName)) setCombineId(slugify(value))
          }} placeholder="Combined prospect pool" /></label>
          <label className="field field--grow"><span>New bucket ID</span><input value={combineId} onChange={(event) => setCombineId(slugify(event.target.value))} placeholder="combined-prospects" /></label>
          <button className="button button--primary" type="button" disabled={!combineSources.size || !combineName.trim() || !combineId || combineMutation.isPending} onClick={() => combineMutation.mutate()}>{combineMutation.isPending ? 'Combining…' : 'Combine'}</button>
        </div>
        {combineMutation.error ? <Notice title="Combine failed">{combineMutation.error.message}</Notice> : null}
      </div>
    </Panel>

    {stats.data ? <Panel title="Schema" eyebrow={numberFormat.format(stats.data.rows) + ' rows · ' + formatBytes(stats.data.total_bytes)}>
      <div className="table-wrap"><table className="data-table"><thead><tr><th>Column</th><th>Type</th><th>Cardinality</th><th>Nullable</th></tr></thead><tbody>{stats.data.column_stats.map((column) => <tr key={column.id}><td className="data-table__primary">{column.name}</td><td><span className="code-chip">{column.logical_type}</span></td><td className="mono">{numberFormat.format(column.cardinality)}</td><td>{column.nullable ? 'Yes' : 'No'}</td></tr>)}</tbody></table></div>
    </Panel> : null}
  </div>
}
