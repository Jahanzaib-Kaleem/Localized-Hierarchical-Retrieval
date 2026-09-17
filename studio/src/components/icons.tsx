import type { SVGProps } from 'react'

type IconProps = SVGProps<SVGSVGElement>

function IconBase({ children, ...props }: IconProps) {
  return (
    <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" {...props}>
      {children}
    </svg>
  )
}

export const OverviewIcon = (props: IconProps) => <IconBase {...props}><path d="M3 3h5v5H3zM12 3h5v8h-5zM3 12h5v5H3zM12 15h5v2h-5z" /></IconBase>
export const HomeIcon = (props: IconProps) => <IconBase {...props}><path d="m3 9 7-6 7 6v8H5V9" /><path d="M8 17v-5h4v5" /></IconBase>
export const DataIcon = (props: IconProps) => <IconBase {...props}><ellipse cx="10" cy="4.5" rx="6.5" ry="2.5" /><path d="M3.5 4.5v5c0 1.4 2.9 2.5 6.5 2.5s6.5-1.1 6.5-2.5v-5M3.5 9.5v5c0 1.4 2.9 2.5 6.5 2.5s6.5-1.1 6.5-2.5v-5" /></IconBase>
export const QueryIcon = (props: IconProps) => <IconBase {...props}><path d="M4 5h12M4 10h8M4 15h5" /><path d="m14 13 3 2-3 2z" /></IconBase>
export const ApiIcon = (props: IconProps) => <IconBase {...props}><path d="m7 5-4 5 4 5M13 5l4 5-4 5M11.5 3 8.5 17" /></IconBase>
export const IndexIcon = (props: IconProps) => <IconBase {...props}><path d="M5 3v14M10 3v14M15 3v14M3 6h14M3 14h14" /></IconBase>
export const WorkloadIcon = (props: IconProps) => <IconBase {...props}><path d="M3 14h2l2-7 3 9 3-12 2 10h2" /></IconBase>
export const GenerationsIcon = (props: IconProps) => <IconBase {...props}><path d="m10 2 7 4-7 4-7-4zM3 10l7 4 7-4M3 14l7 4 7-4" /></IconBase>
export const OperationsIcon = (props: IconProps) => <IconBase {...props}><path d="M10 3v4M10 13v4M3 10h4M13 10h4" /><circle cx="10" cy="10" r="3" /></IconBase>
export const MetricsIcon = (props: IconProps) => <IconBase {...props}><path d="M3 16V9M8 16V5M13 16v-7M18 16V3" /></IconBase>
export const SystemIcon = (props: IconProps) => <IconBase {...props}><rect x="3" y="3" width="14" height="14" rx="2" /><path d="M6 7h8M6 10h8M6 13h5" /></IconBase>
export const SettingsIcon = (props: IconProps) => <IconBase {...props}><circle cx="10" cy="10" r="3" /><path d="M10 2.5v2M10 15.5v2M2.5 10h2M15.5 10h2M4.7 4.7l1.4 1.4M13.9 13.9l1.4 1.4M15.3 4.7l-1.4 1.4M6.1 13.9l-1.4 1.4" /></IconBase>
export const UploadIcon = (props: IconProps) => <IconBase {...props}><path d="M10 13V3M6 7l4-4 4 4" /><path d="M4 12v4h12v-4" /></IconBase>
export const FileIcon = (props: IconProps) => <IconBase {...props}><path d="M5 2.5h6l4 4V17H5z" /><path d="M11 2.5v4h4M7.5 10h5M7.5 13h5" /></IconBase>
export const CopyIcon = (props: IconProps) => <IconBase {...props}><rect x="6" y="6" width="9" height="9" rx="1.5" /><path d="M4 12H3V3h9v1" /></IconBase>
export const ArrowIcon = (props: IconProps) => <IconBase {...props}><path d="m7 4 6 6-6 6" /></IconBase>
export const RefreshIcon = (props: IconProps) => <IconBase {...props}><path d="M16 7a6.5 6.5 0 1 0 .2 5" /><path d="M16 3v4h-4" /></IconBase>
