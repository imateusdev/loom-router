import { Routes, Route } from 'react-router'
import Layout from '@/components/Layout'
import OverviewPage from '@/pages/Overview'
import ProvidersPage from '@/pages/Providers'
import ServerPage from '@/pages/Server'
import CodexPage from '@/pages/Codex'

export default function App() {
  return (
    <Routes>
      <Route element={<Layout />}>
        <Route path="/" element={<OverviewPage />} />
        <Route path="/providers" element={<ProvidersPage />} />
        <Route path="/server" element={<ServerPage />} />
        <Route path="/codex" element={<CodexPage />} />
      </Route>
    </Routes>
  )
}
