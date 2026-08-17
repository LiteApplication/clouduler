// Holds the current identity so views can gate admin-only actions. A 401
// from anywhere routes here too, via setUnauthorizedHandler, so a session
// that expires mid-use drops the user back to the login screen.

import { createContext, useContext, useEffect, useState, type ReactNode } from 'react';
import { api, setUnauthorizedHandler } from '../api/client';
import type { Identity } from '../api/types';

interface AuthState {
  identity: Identity | null;
  loading: boolean;
  isAdmin: boolean;
  refresh: () => Promise<void>;
}

const AuthContext = createContext<AuthState | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [identity, setIdentity] = useState<Identity | null>(null);
  const [loading, setLoading] = useState(true);

  const refresh = async () => {
    try {
      const me = await api.me();
      setIdentity(me);
    } catch {
      setIdentity(null);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    setUnauthorizedHandler(() => setIdentity(null));
    refresh();
    return () => setUnauthorizedHandler(null);
  }, []);

  const value: AuthState = {
    identity,
    loading,
    isAdmin: identity?.role === 'admin',
    refresh,
  };

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthState {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth must be used inside AuthProvider');
  return ctx;
}
