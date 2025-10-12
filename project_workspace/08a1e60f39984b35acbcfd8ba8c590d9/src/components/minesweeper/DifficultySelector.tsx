'use client';

import { GameConfig } from '@/types/minesweeper';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';

interface DifficultySelectorProps {
  currentDifficulty: string;
  onDifficultyChange: (difficulty: string) => void;
}

export function DifficultySelector({ currentDifficulty, onDifficultyChange }: DifficultySelectorProps) {
  const difficulties: Record<string, GameConfig> = {
    beginner: { rows: 9, cols: 9, mines: 10 },
    intermediate: { rows: 16, cols: 16, mines: 40 },
    expert: { rows: 16, cols: 30, mines: 99 },
    custom: { rows: 10, cols: 10, mines: 15 },
  };

  const difficultyInfo = {
    beginner: { name: '初级', description: '9×9 网格，10个地雷', color: 'bg-green-100 text-green-800' },
    intermediate: { name: '中级', description: '16×16 网格，40个地雷', color: 'bg-yellow-100 text-yellow-800' },
    expert: { name: '高级', description: '16×30 网格，99个地雷', color: 'bg-red-100 text-red-800' },
    custom: { name: '自定义', description: '10×10 网格，15个地雷', color: 'bg-blue-100 text-blue-800' },
  };

  return (
    <Card className="p-6">
      <h3 className="text-lg font-semibold mb-4">选择难度</h3>
      <div className="grid grid-cols-2 gap-3">
        {Object.entries(difficulties).map(([key, config]) => {
          const info = difficultyInfo[key];
          return (
            <Button
              key={key}
              variant={currentDifficulty === key ? 'default' : 'outline'}
              className="h-auto p-4 flex flex-col items-start"
              onClick={() => onDifficultyChange(key)}
            >
              <div className="flex items-center justify-between w-full mb-2">
                <span className="font-medium">{info.name}</span>
                {currentDifficulty === key && (
                  <Badge className={info.color} variant="secondary">
                    当前
                  </Badge>
                )}
              </div>
              <div className="text-xs text-left opacity-70">
                {info.description}
              </div>
            </Button>
          );
        })}
      </div>
    </Card>
  );
}