/**
 * 扫雷游戏难度选择组件
 */

import React, { memo, useState } from 'react';
import { Difficulty, GameConfig } from '../../types/minesweeper';
import { getGameConfig } from '../../lib/minesweeper-utils';

interface DifficultySelectorProps {
  currentDifficulty: Difficulty;
  onDifficultyChange: (config: GameConfig) => void;
}

const DifficultySelector: React.FC<DifficultySelectorProps> = memo(({
  currentDifficulty,
  onDifficultyChange
}) => {
  const [showCustom, setShowCustom] = useState(false);
  const [customConfig, setCustomConfig] = useState({
    rows: 16,
    cols: 16,
    mines: 40,
  });

  const difficulties = [
    { value: Difficulty.BEGINNER, label: '初级', description: '9×9, 10个雷' },
    { value: Difficulty.INTERMEDIATE, label: '中级', description: '16×16, 40个雷' },
    { value: Difficulty.EXPERT, label: '高级', description: '16×30, 99个雷' },
    { value: Difficulty.CUSTOM, label: '自定义', description: '自定义设置' },
  ];

  const handleDifficultySelect = (difficulty: Difficulty) => {
    if (difficulty === Difficulty.CUSTOM) {
      setShowCustom(true);
    } else {
      setShowCustom(false);
      const config = getGameConfig(difficulty);
      onDifficultyChange(config);
    }
  };

  const handleCustomConfigSubmit = (e: React.FormEvent) => {
    e.preventDefault();

    // 验证自定义配置
    const maxMines = Math.floor(customConfig.rows * customConfig.cols * 0.8); // 最多80%是雷

    if (customConfig.mines < 1 || customConfig.mines > maxMines) {
      alert(`地雷数量必须在 1 到 ${maxMines} 之间`);
      return;
    }

    if (customConfig.rows < 5 || customConfig.rows > 30) {
      alert('行数必须在 5 到 30 之间');
      return;
    }

    if (customConfig.cols < 5 || customConfig.cols > 30) {
      alert('列数必须在 5 到 30 之间');
      return;
    }

    const config = getGameConfig(Difficulty.CUSTOM, customConfig);
    onDifficultyChange(config);
    setShowCustom(false);
  };

  return (
    <div className="bg-white rounded-lg shadow-lg p-6 max-w-md mx-auto">
      <h3 className="text-lg font-bold mb-4 text-gray-800">选择难度</h3>

      <div className="space-y-2">
        {difficulties.map((diff) => (
          <button
            key={diff.value}
            onClick={() => handleDifficultySelect(diff.value)}
            className={`w-full text-left px-4 py-3 rounded-lg border-2 transition-all duration-150 ${
              currentDifficulty === diff.value && !showCustom
                ? 'border-blue-500 bg-blue-50 text-blue-700'
                : 'border-gray-200 hover:border-gray-300 hover:bg-gray-50'
            }`}
          >
            <div className="font-medium">{diff.label}</div>
            <div className="text-sm text-gray-600">{diff.description}</div>
          </button>
        ))}
      </div>

      {showCustom && (
        <form onSubmit={handleCustomConfigSubmit} className="mt-4 space-y-3 border-t pt-4">
          <h4 className="font-medium text-gray-800">自定义设置</h4>

          <div className="grid grid-cols-3 gap-3">
            <div>
              <label className="block text-sm font-medium text-gray-700 mb-1">
                行数
              </label>
              <input
                type="number"
                min="5"
                max="30"
                value={customConfig.rows}
                onChange={(e) => setCustomConfig({ ...customConfig, rows: parseInt(e.target.value) || 5 })}
                className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
              />
            </div>

            <div>
              <label className="block text-sm font-medium text-gray-700 mb-1">
                列数
              </label>
              <input
                type="number"
                min="5"
                max="30"
                value={customConfig.cols}
                onChange={(e) => setCustomConfig({ ...customConfig, cols: parseInt(e.target.value) || 5 })}
                className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
              />
            </div>

            <div>
              <label className="block text-sm font-medium text-gray-700 mb-1">
                地雷数
              </label>
              <input
                type="number"
                min="1"
                max={Math.floor(customConfig.rows * customConfig.cols * 0.8)}
                value={customConfig.mines}
                onChange={(e) => setCustomConfig({ ...customConfig, mines: parseInt(e.target.value) || 1 })}
                className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
              />
            </div>
          </div>

          <div className="flex space-x-2">
            <button
              type="submit"
              className="flex-1 bg-blue-500 text-white px-4 py-2 rounded-md hover:bg-blue-600 transition-colors duration-150"
            >
              开始游戏
            </button>
            <button
              type="button"
              onClick={() => setShowCustom(false)}
              className="flex-1 bg-gray-300 text-gray-700 px-4 py-2 rounded-md hover:bg-gray-400 transition-colors duration-150"
            >
              取消
            </button>
          </div>
        </form>
      )}
    </div>
  );
});

DifficultySelector.displayName = 'DifficultySelector';

export default DifficultySelector;