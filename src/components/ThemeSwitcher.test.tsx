// Guards the wiring the store test cannot see: the button cycles through all
// three themes and each one has a translated label to announce.

import { describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import ThemeSwitcher from './ThemeSwitcher'
import { setTheme } from '@/lib/theme'

describe('ThemeSwitcher', () => {
  it('cycles system -> light -> dark and back', async () => {
    setTheme('system')
    render(<ThemeSwitcher />)
    const button = () => screen.getByRole('button')

    expect(button()).toHaveAccessibleName('Theme: system')
    await userEvent.click(button())
    expect(button()).toHaveAccessibleName('Theme: light')
    expect(document.documentElement).not.toHaveClass('dark')

    await userEvent.click(button())
    expect(button()).toHaveAccessibleName('Theme: dark')
    expect(document.documentElement).toHaveClass('dark')

    await userEvent.click(button())
    expect(button()).toHaveAccessibleName('Theme: system')
    expect(document.documentElement).not.toHaveClass('dark')
  })
})
