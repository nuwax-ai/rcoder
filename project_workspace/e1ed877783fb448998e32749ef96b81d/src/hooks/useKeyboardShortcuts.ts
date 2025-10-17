import { useEffect } from 'react';
import { GameConfig, DIFFICULTY_LEVELS } from '../types/minesweeper';

interface UseKeyboardShortcutsProps {
  onNewGame: () => void;
  onDifficultyChange: (difficulty: string, config: GameConfig) => void;
}

export const useKeyboardShortcuts = ({ onNewGame, onDifficultyChange }: UseKeyboardShortcutsProps) => {
  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      // 防止在输入框中触发快捷键
      const target = event.target as HTMLElement;
      if (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA') {
        return;
      }

      const key = event.key.toLowerCase();

      switch (key) {
        case 'r':
          event.preventDefault();
          onNewGame();
          break;
        case '1':
          event.preventDefault();
          onDifficultyChange('beginner', DIFFICULTY_LEVELS.beginner);
          break;
        case '2':
          event.preventDefault();
          onDifficultyChange('intermediate', DIFFICULTY_LEVELS.intermediate);
          break;
        case '3':
          event.preventDefault();
          onDifficultyChange('expert', DIFFICULTY_LEVELS.expert);
          break;
      }
    };

    window.addEventListener('keydown', handleKeyDown);

    return () => {
      window.removeEventListener('keydown', handleKeyDown);
    };
  }, [onNewGame, onDifficultyChange]);
};