import { useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api } from '../../api/client'
import type { ImportDatasetSchema, LogicalType, Normalization } from '../../api/types'
import { formatBytes, numberFormat } from '../../components/format'
import { FileIcon, UploadIcon } from '../../components/icons'
import { Notice, PageHeader, Panel } from '../../components/ui'
import { previewCsv, type CsvPreview } from './csv'

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

export function DataPage() {
  const queryClient = useQueryClient()
  const inputRef = useRef<HTMLInputElement>(null)
  const stats = useQuery({ queryKey: ['stats'], queryFn: ({ signal }) => api.stats(signal), staleTime: 30_000, retry: false })
  const [file, setFile] = useState<File | null>(null)
  const [preview, setPreview] = useState<CsvPreview | null>(null)
  const [fileError, setFileError] = useState('')
  const [dragging, setDragging] = useState(false)

  const selectFile = async (next: File) => {
    setFileError('')
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
      return api.importCsv(file, preview.schema)
    },
    onSuccess: async () => {
      setFile(null)
      setPreview(null)
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['stats'] }),
        queryClient.invalidateQueries({ queryKey: ['ready'] }),
        queryClient.invalidateQueries({ queryKey: ['generations'] }),
      ])
    },
  })

  const startImport = () => {
    if (!file || !preview) return
    if (stats.data && !window.confirm('Importing this CSV will publish it as the new current dataset. The previous generation remains available for recovery. Continue?')) return
    importData.mutate()
  }

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Dataset" title="Data" description="Import a CSV, review its schema, and keep the current dataset understandable at a glance." />

    <Panel title={stats.data ? 'Import or replace dataset' : 'Import your first dataset'} eyebrow="CSV">
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
          <input ref={inputRef} className="visually-hidden" type="file" accept=".csv,text/csv" onChange={(event) => { const selected = event.target.files?.[0]; if (selected) void selectFile(selected); event.currentTarget.value = '' }} />
          <div className="drop-zone__icon">{file ? <FileIcon /> : <UploadIcon />}</div>
          <div className="drop-zone__copy">
            <strong>{file ? file.name : 'Drop a CSV here'}</strong>
            <span>{file ? `${formatBytes(file.size)} · click to choose another file` : 'or click to choose a file'}</span>
          </div>
          <span className="button button--quiet">Choose CSV</span>
        </div>
        {fileError ? <Notice title="Could not read CSV">{fileError}</Notice> : null}
        <p className="field-hint">Studio previews only a small sample in the browser. The actual import is streamed to LHR and built on the server.</p>
      </div>
    </Panel>

    {preview ? <>
      <Panel title="Review columns" eyebrow={`${preview.schema.columns.length} detected`} action={<button className="button button--primary" disabled={importData.isPending} type="button" onClick={startImport}>{importData.isPending ? 'Importing…' : stats.data ? 'Import as new dataset' : 'Create dataset'}</button>}>
        <div className="table-wrap schema-editor-wrap"><table className="data-table schema-editor"><thead><tr><th>Column</th><th>Type</th><th>Nullable</th><th>Normalization</th></tr></thead><tbody>
          {preview.schema.columns.map((column, index) => <tr key={`${column.name}-${index}`}>
            <td className="data-table__primary">{column.name}</td>
            <td><select value={column.logical_type} onChange={(event) => updateSchema((schema) => ({ ...schema, columns: schema.columns.map((item, i) => i === index ? { ...item, logical_type: event.target.value as LogicalType } : item) }))}>{logicalTypes.map((type) => <option value={type.value} key={type.value}>{type.label}</option>)}</select></td>
            <td><label className="toggle-field"><input type="checkbox" checked={column.nullable} onChange={(event) => updateSchema((schema) => ({ ...schema, columns: schema.columns.map((item, i) => i === index ? { ...item, nullable: event.target.checked, null_values: event.target.checked ? [''] : [] } : item) }))} /><span>{column.nullable ? 'Yes' : 'No'}</span></label></td>
            <td><select value={column.normalization} onChange={(event) => updateSchema((schema) => ({ ...schema, columns: schema.columns.map((item, i) => i === index ? { ...item, normalization: event.target.value as Normalization } : item) }))}>{normalizations.map((normalization) => <option value={normalization.value} key={normalization.value}>{normalization.label}</option>)}</select></td>
          </tr>)}
        </tbody></table></div>
        {importData.error ? <Notice title="Import failed">{importData.error.message}</Notice> : null}
        {importData.data ? <Notice title="Dataset imported">Published generation {importData.data.generation.id} with {numberFormat.format(importData.data.rows)} rows.</Notice> : null}
      </Panel>

      <Panel title="Preview" eyebrow="First complete rows">
        <div className="table-wrap preview-table-wrap"><table className="data-table"><thead><tr>{preview.headers.map((header) => <th key={header}>{header}</th>)}</tr></thead><tbody>{preview.rows.map((row, rowIndex) => <tr key={rowIndex}>{preview.headers.map((header, columnIndex) => <td key={`${rowIndex}-${header}`} title={row[columnIndex] ?? ''}>{row[columnIndex] || <span className="null-value">empty</span>}</td>)}</tr>)}</tbody></table></div>
      </Panel>
    </> : null}

    {stats.data ? <Panel title="Current schema" eyebrow={`${numberFormat.format(stats.data.rows)} rows · ${formatBytes(stats.data.total_bytes)}`}>
      <div className="table-wrap"><table className="data-table"><thead><tr><th>Column</th><th>Type</th><th>Cardinality</th><th>Nullable</th></tr></thead><tbody>{stats.data.column_stats.map((column) => <tr key={column.id}><td className="data-table__primary">{column.name}</td><td><span className="code-chip">{column.logical_type}</span></td><td className="mono">{numberFormat.format(column.cardinality)}</td><td>{column.nullable ? 'Yes' : 'No'}</td></tr>)}</tbody></table></div>
    </Panel> : null}
  </div>
}
