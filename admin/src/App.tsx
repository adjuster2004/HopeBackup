import React from 'react'
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom'
import { auth } from './api'
import { Layout } from './components/Layout'
import { ToastProvider } from './components/ui'
import { MailPage } from './pages/MailPage'
import { RecoverPage, ResetPasswordPage } from './pages/RecoverPage'
import { LoginPage } from './pages/LoginPage'
import { DashboardPage } from './pages/DashboardPage'
import { ProductsPage } from './pages/ProductsPage'
import { AddProductPage } from './pages/AddProductPage'
import { InstancePage } from './pages/InstancePage'
import { RestorePage } from './pages/RestorePage'
import { SitesPage } from './pages/SitesPage'
import { ConnectorsPage } from './pages/ConnectorsPage'
import { AgentsPage } from './pages/AgentsPage'
import { TargetsPage } from './pages/TargetsPage'
import { MigrationsPage } from './pages/MigrationsPage'
import { UpdatesPage } from './pages/UpdatesPage'
import { JobsPage } from './pages/JobsPage'
import { AuditPage } from './pages/AuditPage'
import { UsersPage } from './pages/UsersPage'
import { ClusterPage } from './pages/ClusterPage'
import { SettingsPage } from './pages/SettingsPage'
import { MachinePage, MachinesPage } from './pages/MachinesPage'

const Guard = ({ children }: { children: React.ReactNode }) => auth.token() ? <Layout>{children}</Layout> : <Navigate to="/login" replace />

export const App = () => (
  <ToastProvider>
    <BrowserRouter>
      <Routes>
        <Route path="/login" element={<LoginPage />} />
        <Route path="/recover" element={<RecoverPage />} />
        <Route path="/reset-password" element={<ResetPasswordPage />} />
        <Route path="/mail" element={<Guard><MailPage /></Guard>} />
        <Route path="/dashboard" element={<Guard><DashboardPage /></Guard>} />
        <Route path="/products" element={<Guard><ProductsPage /></Guard>} />
        <Route path="/products/new" element={<Guard><AddProductPage /></Guard>} />
        <Route path="/instances/:id" element={<Guard><InstancePage /></Guard>} />
        <Route path="/restore" element={<Guard><RestorePage /></Guard>} />
        <Route path="/sites" element={<Guard><SitesPage /></Guard>} />
        <Route path="/connectors" element={<Guard><ConnectorsPage /></Guard>} />
        <Route path="/agents" element={<Guard><AgentsPage /></Guard>} />
        <Route path="/targets" element={<Guard><TargetsPage /></Guard>} />
        <Route path="/migrations" element={<Guard><MigrationsPage /></Guard>} />
        <Route path="/updates" element={<Guard><UpdatesPage /></Guard>} />
        <Route path="/jobs" element={<Guard><JobsPage /></Guard>} />
        <Route path="/audit" element={<Guard><AuditPage /></Guard>} />
        <Route path="/users" element={<Guard><UsersPage /></Guard>} />
        <Route path="/cluster" element={<Guard><ClusterPage /></Guard>} />
        <Route path="/settings" element={<Guard><SettingsPage /></Guard>} />
        <Route path="/machines" element={<Guard><MachinesPage /></Guard>} />
        <Route path="/machines/:id" element={<Guard><MachinePage /></Guard>} />
        <Route path="*" element={<Navigate to={auth.token() ? '/dashboard' : '/login'} replace />} />
      </Routes>
    </BrowserRouter>
  </ToastProvider>
)
