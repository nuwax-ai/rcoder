import React from 'react';
import { GameConfig, DIFFICULTY_LEVELS } from '../types/minesweeper';

interface DifficultySelectorProps {
  currentDifficulty: string;
  onDifficultyChange: (difficulty: string, config: GameConfig) => void;
}

export const DifficultySelector: React.FC<DifficultySelectorProps> = ({
  currentDifficulty,
  onDifficultyChange,
}) => {
  const difficulties = [
    { key: 'beginner', name: '初级', description: '9×9, 10个地雷' },
    { key: 'intermediate', name: '中级', description: '16×16, 40个地雷' },
    { key: 'expert', name: '高级', description: '16×30, 99个地雷' },
  ];

  return (
    <div className="mb-6">
      <h3 className="text-lg font-semibold text-center mb-3">选择难度</h3>
      <div className="flex justify-center gap-3 flex-wrap">
        {difficulties.map((difficulty) => (
          <button
            key={difficulty.key}
            onClick={() => onDifficultyChange(difficulty.key, DIFFICULTY_LEVELS[difficulty.key])}
            className={`px-4 py-2 rounded-lg font-medium transition-all duration-200 ${
              currentDifficulty === difficulty.key
                ? 'bg-blue-500 text-white shadow-lg transform scale-105'
                : 'bg-gray-200 text-gray-700 hover:bg-gray-300'
            }`}
          >
            <div className="text-center">
              <div className="font-bold">{difficulty.name}</div>
              <div className="text-xs opacity-80">{difficulty.description}</div>
            </div>
          </button>
        ))}
      </div>
    </div>
  );
};